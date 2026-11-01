//! Port of `src/terminal/ref_counted_set.zig`.
//!
//! STATUS: ported

use core::fmt;
use core::mem::MaybeUninit;

use crate::terminal::size::{align_forward, Base, Offset, OffsetBuf};

/// Behaviors for a [`RefCountedSet`] (Zig: the `Context` type parameter).
///
/// - `hash` - Return a hash for an item.
///
/// - `eql` - Check two items for equality.
///   The first of the two items passed in is guaranteed to be from
///   a value passed in to an `add` or `lookup` function, the second
///   is guaranteed to be a value already resident in the set.
///
/// - `deleted` - [OPTIONAL] Deletion callback.
///   Called whenever an item is finally deleted.
///   Useful if the item has memory that needs to be freed.
///   The default implementation does nothing (Zig: decl not present).
pub trait RefCountedSetContext<T> {
    fn hash(&self, value: &T) -> u64;
    fn eql(&self, a: &T, b: &T) -> bool;
    fn deleted(&self, _value: &T) {}
}

/// Unsigned integer types usable as the set's `Id` and ref count types
/// (Zig: any unsigned integer type via comptime).
pub trait SetInt:
    Copy
    + Ord
    + Default
    + fmt::Debug
    + core::ops::Add<Output = Self>
    + core::ops::Sub<Output = Self>
    + core::ops::AddAssign
    + core::ops::SubAssign
{
    const ZERO: Self;
    const ONE: Self;
    const MAX: Self;
    fn to_usize(self) -> usize;
    /// Truncating conversion (Zig `@intCast` where the value is known to fit).
    fn from_usize(v: usize) -> Self;
}

macro_rules! set_int {
    ($($t:ty),*) => { $(
        impl SetInt for $t {
            const ZERO: Self = 0;
            const ONE: Self = 1;
            const MAX: Self = <$t>::MAX;
            #[inline(always)]
            fn to_usize(self) -> usize { self as usize }
            #[inline(always)]
            fn from_usize(v: usize) -> Self {
                debug_assert!(v as u128 <= <$t>::MAX as u128);
                v as $t
            }
        }
    )* };
}
set_int!(u8, u16, u32, u64, usize);

/// Metadata for a set item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemMetadata<Id, R> {
    /// The bucket in the hash table where this item
    /// is referenced.
    pub bucket: Id,

    /// The length of the probe sequence between this
    /// item's starting bucket and the bucket it's in,
    /// used for Robin Hood hashing.
    pub psl: Id,

    /// The reference count for this item.
    pub r#ref: R,
}

impl<Id: SetInt, R: SetInt> Default for ItemMetadata<Id, R> {
    fn default() -> Self {
        Self {
            bucket: Id::MAX,
            psl: Id::ZERO,
            r#ref: R::ZERO,
        }
    }
}

/// Set item
pub struct Item<T, Id, R> {
    /// The value this item represents. Undefined (uninitialized) for
    /// unused/reaped items.
    pub value: MaybeUninit<T>,

    /// Metadata for this item.
    pub meta: ItemMetadata<Id, R>,
}

impl<T: Copy, Id: Copy, R: Copy> Clone for Item<T, Id, R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Copy, Id: Copy, R: Copy> Copy for Item<T, Id, R> {}

impl<T, Id: SetInt, R: SetInt> Default for Item<T, Id, R> {
    fn default() -> Self {
        Self {
            value: MaybeUninit::uninit(),
            meta: ItemMetadata::default(),
        }
    }
}

/// The memory layout of a [`RefCountedSet`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout<Id> {
    pub cap: usize,
    pub table_cap: usize,
    pub table_mask: Id,
    pub table_start: usize,
    pub items_start: usize,
    pub total_size: usize,
}

/// Possible errors for `add` and `addWithId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddError {
    /// There is not enough memory to add a new item.
    /// Remove items or grow and reinitialize.
    OutOfMemory,

    /// The set needs to be rehashed, as there are many dead
    /// items with lower IDs which are inaccessible for reuse.
    NeedsRehash,
}

impl fmt::Display for AddError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AddError::OutOfMemory => f.write_str("OutOfMemory"),
            AddError::NeedsRehash => f.write_str("NeedsRehash"),
        }
    }
}

impl std::error::Error for AddError {}

/// A live entry returned by [`SetIterator`].
pub struct Entry<T, Id> {
    pub id: Id,
    pub value_ptr: *mut T,
}

/// Iterates live entries in ascending ID order.
/// (Zig: `RefCountedSet.Iterator`.)
///
/// Released entries whose reference count reached zero are skipped.
/// Any mutation of the set invalidates the iterator.
pub struct SetIterator<T, Id, R> {
    items: *mut Item<T, Id, R>,
    id: Id,
    end: Id,
}

impl<T, Id: SetInt, R: SetInt> Iterator for SetIterator<T, Id, R> {
    type Item = Entry<T, Id>;

    fn next(&mut self) -> Option<Entry<T, Id>> {
        while self.id < self.end {
            let id = self.id;
            self.id += Id::ONE;

            // SAFETY: ids below `end` (= next_id) are within the items array
            // (contract of `RefCountedSet::iterator`).
            let item = unsafe { self.items.add(id.to_usize()) };
            if unsafe { (*item).meta.r#ref } == R::ZERO {
                continue;
            }

            return Some(Entry {
                id,
                value_ptr: unsafe { core::ptr::addr_of_mut!((*item).value) }.cast::<T>(),
            });
        }

        None
    }
}

/// A reference counted set.
///
/// This set is created with some capacity in mind. You can determine
/// the exact memory requirement of a given capacity by calling `layout`
/// and checking the total size.
///
/// When the set exceeds capacity, an `OutOfMemory` or `NeedsRehash` error
/// is returned from any memory-using methods. The caller is responsible
/// for determining a path forward.
///
/// This set is reference counted. Each item in the set has an associated
/// reference count. The caller is responsible for calling release for an
/// item when it is no longer being used. Items with 0 references will be
/// kept until another item is written to their bucket. This allows items
/// to be resurrected if they are re-added before they get overwritten.
///
/// The backing data structure of this set is an open addressed hash table
/// with linear probing and Robin Hood hashing, and a flat array of items.
///
/// The table maps values to item IDs, which are indices in the item array
/// which contain the item's value and its reference count. Item IDs can be
/// used to efficiently access an item and update its reference count after
/// it has been added to the table, to avoid having to use the hash map to
/// look the value back up.
///
/// ID 0 is reserved and will never be assigned.
///
/// # Safety model
///
/// The set stores only offsets. Every method that touches the backing
/// memory takes the base pointer and is `unsafe`: `base` must be the true
/// base of the buffer the set was initialized in (it may have been moved or
/// copied since), valid for reads and writes of `layout.total_size` bytes
/// past the set's [`OffsetBuf`], and not accessed concurrently.
pub struct RefCountedSet<T, Id, R, Ctx> {
    /// A hash table of item indices
    pub table: Offset<Id>,

    /// By keeping track of the max probe sequence length
    /// we can bail out early when looking up values that
    /// aren't present.
    pub max_psl: Id,

    /// We keep track of how many items have a PSL of any
    /// given length, so that we can shrink max_psl when
    /// we delete items.
    ///
    /// A probe sequence of length 32 or more is astronomically
    /// unlikely. Roughly a (1/table_cap)^32 -- with any normal
    /// table capacity that is so unlikely that it's not worth
    /// handling.
    ///
    /// However, that assumes a uniform hash function, which
    /// is not guaranteed and can be subverted with a crafted
    /// input. We handle this gracefully by returning an error
    /// anywhere where we're about to insert if there's any
    /// item with a PSL in the last slot of the stats array.
    pub psl_stats: [Id; 32],

    /// The backing store of items
    pub items: Offset<Item<T, Id, R>>,

    /// The number of living items currently stored in the set.
    pub living: usize,

    /// The next index to store an item at.
    /// Id 0 is reserved for unused items.
    pub next_id: Id,

    pub layout: Layout<Id>,

    /// An instance of the context structure.
    pub context: Ctx,
}

impl<T, Id: Clone, R, Ctx: Clone> Clone for RefCountedSet<T, Id, R, Ctx> {
    fn clone(&self) -> Self {
        Self {
            table: self.table,
            max_psl: self.max_psl.clone(),
            psl_stats: self.psl_stats.clone(),
            items: self.items,
            living: self.living,
            next_id: self.next_id.clone(),
            layout: self.layout.clone(),
            context: self.context.clone(),
        }
    }
}

impl<T, Id: Copy, R, Ctx: Copy> Copy for RefCountedSet<T, Id, R, Ctx> {}

impl<T, Id: fmt::Debug, R, Ctx> fmt::Debug for RefCountedSet<T, Id, R, Ctx> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RefCountedSet")
            .field("table", &self.table.offset)
            .field("max_psl", &self.max_psl)
            .field("items", &self.items.offset)
            .field("living", &self.living)
            .field("next_id", &self.next_id)
            .field("layout", &self.layout)
            .finish_non_exhaustive()
    }
}

impl<T, Id, R, Ctx> RefCountedSet<T, Id, R, Ctx>
where
    T: Copy,
    Id: SetInt,
    R: SetInt,
    Ctx: RefCountedSetContext<T> + Clone,
{
    pub const BASE_ALIGN: usize = {
        let mut a = core::mem::align_of::<Ctx>();
        if core::mem::align_of::<Layout<Id>>() > a {
            a = core::mem::align_of::<Layout<Id>>();
        }
        if core::mem::align_of::<Item<T, Id, R>>() > a {
            a = core::mem::align_of::<Item<T, Id, R>>();
        }
        if core::mem::align_of::<Id>() > a {
            a = core::mem::align_of::<Id>();
        }
        a
    };

    /// This is the max load until the set returns OutOfMemory and
    /// requires more capacity.
    ///
    /// Experimentally, this load factor works quite well.
    pub const LOAD_FACTOR: f64 = 0.8125;

    /// LOAD_FACTOR as an exact fraction.
    const LOAD_FACTOR_NUM: usize = 13;
    const LOAD_FACTOR_DEN: usize = 16;
    const LOAD_FACTOR_IS_FRACTION: () =
        assert!(Self::LOAD_FACTOR_NUM as f64 / Self::LOAD_FACTOR_DEN as f64 == Self::LOAD_FACTOR);

    /// `@intFromFloat(load_factor * @floatFromInt(table_cap))` for a
    /// power-of-two table capacity, in integer arithmetic. The float
    /// product is exact (LOAD_FACTOR is 13/16 and table_cap is far below
    /// 2^53), so truncating it equals the floor division. The int-to-float
    /// conversion was the critical path of page layout computations (Zig's
    /// LLVM builds the power of two's float from its exponent instead).
    #[inline(always)]
    fn items_cap(table_cap: usize) -> usize {
        #[allow(clippy::let_unit_value)]
        let () = Self::LOAD_FACTOR_IS_FRACTION;
        debug_assert!(table_cap.is_power_of_two() && table_cap <= 1 << 52);
        table_cap * Self::LOAD_FACTOR_NUM / Self::LOAD_FACTOR_DEN
    }

    /// Returns the minimum capacity needed to store `n` items,
    /// accounting for the load factor and the reserved ID 0.
    pub fn capacity_for_count(n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        // +1 because ID 0 is reserved, so we need at least n+1 slots.
        ((n + 1) as f64 / Self::LOAD_FACTOR).ceil() as usize
    }

    /// Returns the memory layout for the given base offset and
    /// desired capacity. The layout can be used by the caller to
    /// determine how much memory to allocate, and the layout must
    /// be used to initialize the set so that the set knows all
    /// the offsets for the various buffers.
    ///
    /// The capacity passed for cap will be used for the hash table,
    /// which has a load factor of `0.8125` (13/16), so the number of
    /// items which can actually be stored in the set will be smaller.
    ///
    /// The laid out capacity will be at least `cap`, but may be higher,
    /// since it is rounded up to the next power of 2 for efficiency.
    ///
    /// The returned layout `cap` property will be 1 more than the number
    /// of items that the set can actually store, since ID 0 is reserved.
    ///
    /// (Zig: `Layout.init(cap)`.)
    pub fn layout(cap: usize) -> Layout<Id> {
        assert!(cap <= Id::MAX.to_usize() + 1);

        // Zero-cap set is valid, return special case
        if cap == 0 {
            return Layout {
                cap: 0,
                table_cap: 0,
                table_mask: Id::ZERO,
                table_start: 0,
                items_start: 0,
                total_size: 0,
            };
        }

        let table_cap: usize = cap.next_power_of_two();
        let items_cap: usize = Self::items_cap(table_cap);

        let table_mask: Id = Id::from_usize((1usize << table_cap.ilog2()) - 1);

        let table_start = 0;
        let table_end = table_start + table_cap * core::mem::size_of::<Id>();

        let items_start = align_forward(table_end, core::mem::align_of::<Item<T, Id, R>>());
        let items_end = items_start + items_cap * core::mem::size_of::<Item<T, Id, R>>();

        let total_size = items_end;

        Layout {
            cap: items_cap,
            table_cap,
            table_mask,
            table_start,
            items_start,
            total_size,
        }
    }

    /// Initialize a set in the given buffer.
    ///
    /// # Safety
    ///
    /// `base.start()` must point to at least `l.total_size` writable bytes
    /// aligned to [`Self::BASE_ALIGN`].
    pub unsafe fn init(base: OffsetBuf, l: Layout<Id>, context: Ctx) -> Self {
        let table = base.member::<Id>(l.table_start);
        let items = base.member::<Item<T, Id, R>>(l.items_start);

        // SAFETY: both regions are within the buffer per the caller contract.
        unsafe {
            let table_ptr = table.ptr(base);
            for i in 0..l.table_cap {
                table_ptr.add(i).write(Id::ZERO);
            }
            let items_ptr = items.ptr(base);
            for i in 0..l.cap {
                items_ptr.add(i).write(Item::default());
            }
        }

        Self::init_from_parts(table, items, l, context)
    }

    /// Like `init`, but for backing memory that the caller guarantees
    /// is already zero-filled (e.g. fresh OS pages). This writes
    /// nothing to the backing buffer, so the OS pages behind the table
    /// and items stay untouched until the first `add`.
    ///
    /// # Safety
    ///
    /// Same as [`Self::init`]; behavior is undefined if the backing memory
    /// is not zero.
    pub unsafe fn init_assume_zeroed(base: OffsetBuf, l: Layout<Id>, context: Ctx) -> Self {
        Self::init_from_parts(
            base.member::<Id>(l.table_start),
            base.member::<Item<T, Id, R>>(l.items_start),
            l,
            context,
        )
    }

    fn init_from_parts(table: Offset<Id>, items: Offset<Item<T, Id, R>>, l: Layout<Id>, context: Ctx) -> Self {
        Self {
            table,
            max_psl: Id::ZERO,
            psl_stats: [Id::ZERO; 32],
            items,
            living: 0,
            next_id: Id::ONE,
            layout: l,
            context,
        }
    }

    // Raw accessors. SAFETY for all: `base` is the set's true base and the
    // index is within the respective array (ids < layout.cap, buckets <
    // layout.table_cap).
    #[inline(always)]
    unsafe fn tbl(&self, base: *mut u8, bucket: usize) -> *mut Id {
        debug_assert!(bucket < self.layout.table_cap);
        unsafe { self.table.ptr(base).add(bucket) }
    }

    #[inline(always)]
    unsafe fn item(&self, base: *mut u8, id: Id) -> *mut Item<T, Id, R> {
        debug_assert!(id.to_usize() < self.layout.cap);
        unsafe { self.items.ptr(base).add(id.to_usize()) }
    }

    /// Add an item to the set if not present and increment its ref count.
    ///
    /// Returns the item's ID.
    ///
    /// If the set has no more room, then an OutOfMemory error is returned.
    ///
    /// # Safety
    ///
    /// See the type-level safety model.
    pub unsafe fn add(&mut self, base: impl Base, value: T) -> Result<Id, AddError> {
        let ctx = self.context.clone();
        // SAFETY: forwarded.
        unsafe { self.add_context(base, value, &ctx) }
    }

    /// # Safety
    ///
    /// See the type-level safety model.
    pub unsafe fn add_context(&mut self, base: impl Base, value: T, ctx: &Ctx) -> Result<Id, AddError> {
        let base = base.base_ptr();

        unsafe {
            // Trim dead items from the end of the list.
            while self.next_id > Id::ONE && (*self.item(base, self.next_id - Id::ONE)).meta.r#ref == R::ZERO {
                self.next_id -= Id::ONE;
                self.delete_item(base, self.next_id, ctx);
            }

            // If the item already exists, return it.
            if let Some(id) = self.lookup_context(base, value, ctx) {
                // Notify the context that the value is "deleted" because
                // we're reusing the existing value in the set. This allows
                // callers to clean up any resources associated with the value.
                ctx.deleted(&value);

                (*self.item(base, id)).meta.r#ref += R::ONE;
                return Ok(id);
            }
        }

        // While it should be statistically impossible to exceed the
        // bounds of `psl_stats`, the hash function is not perfect and
        // in such a case we want to remain stable. If we're about to
        // insert an item and there's something with a PSL of `len - 1`,
        // we may end up with a PSL of `len` which would exceed the bounds.
        // In such a case, we claim to be out of memory.
        if self.psl_stats[self.psl_stats.len() - 1] > Id::ZERO {
            return Err(AddError::OutOfMemory);
        }

        // If the item doesn't exist, we need an available ID.
        if self.next_id.to_usize() >= self.layout.cap {
            // Arbitrarily chosen, threshold for rehashing.
            // If less than 90% of currently allocated IDs
            // correspond to living items, we should rehash.
            // Otherwise, claim we're out of memory because
            // we assume that we'll end up running out of
            // memory or rehashing again very soon if we
            // rehash with only a few IDs left.
            const REHASH_THRESHOLD: f64 = 0.9;
            let threshold = Id::from_usize((self.layout.cap as f64 * REHASH_THRESHOLD) as usize);
            if self.living < threshold.to_usize() {
                return Err(AddError::NeedsRehash);
            }

            // If we don't have at least 10% dead items then
            // we claim we're out of memory.
            return Err(AddError::OutOfMemory);
        }

        unsafe {
            let id = self.insert(base, value, self.next_id, ctx);
            let item = self.item(base, id);
            (*item).meta.r#ref += R::ONE;
            debug_assert!((*item).meta.r#ref == R::ONE);
            self.living += 1;

            // Its possible insert returns a different ID by reusing a
            // dead item so we only need to update next id if we used it.
            if id == self.next_id {
                self.next_id += Id::ONE;
            }

            Ok(id)
        }
    }

    /// Add an item to the set if not present and increment its
    /// ref count. If possible, use the provided ID.
    ///
    /// Returns the item's ID, or null if the provided ID was used.
    ///
    /// If the set has no more room, then an OutOfMemory error is returned.
    ///
    /// # Safety
    ///
    /// See the type-level safety model.
    pub unsafe fn add_with_id(&mut self, base: impl Base, value: T, id: Id) -> Result<Option<Id>, AddError> {
        let ctx = self.context.clone();
        // SAFETY: forwarded.
        unsafe { self.add_with_id_context(base, value, id, &ctx) }
    }

    /// # Safety
    ///
    /// See the type-level safety model.
    pub unsafe fn add_with_id_context(
        &mut self,
        base: impl Base,
        value: T,
        id: Id,
        ctx: &Ctx,
    ) -> Result<Option<Id>, AddError> {
        let base = base.base_ptr();

        assert!(id > Id::ZERO);

        if id < self.next_id {
            unsafe {
                let item = self.item(base, id);
                if (*item).meta.r#ref == R::ZERO {
                    // Requested ID is dead, but if the value exists not under
                    // another ID then ref count that and increase the ID.
                    if let Some(existing_id) = self.lookup_context(base, value, ctx) {
                        // Notify the context that the value is "deleted"
                        // because we're reusing the existing value in the
                        // set. This allows callers to clean up any
                        // resources associated with the value.
                        ctx.deleted(&value);

                        (*self.item(base, existing_id)).meta.r#ref += R::ONE;
                        return Ok(Some(existing_id));
                    }

                    // See comment in `addContext` for details.
                    if self.psl_stats[self.psl_stats.len() - 1] > Id::ZERO {
                        return Err(AddError::OutOfMemory);
                    }

                    self.delete_item(base, id, ctx);
                    let added_id = self.insert(base, value, id, ctx);

                    (*self.item(base, added_id)).meta.r#ref += R::ONE;

                    self.living += 1;

                    return Ok(if added_id == id { None } else { Some(added_id) });
                } else if ctx.eql(&value, (*item).value.assume_init_ref()) {
                    // Notify the context that the value is "deleted" because
                    // we're reusing the existing value in the set. This allows
                    // callers to clean up any resources associated with the value.
                    ctx.deleted(&value);

                    (*item).meta.r#ref += R::ONE;

                    return Ok(None);
                }
            }
        }

        // SAFETY: forwarded.
        unsafe { self.add_context(base, value, ctx).map(Some) }
    }

    /// Increment an item's reference count by 1.
    ///
    /// Asserts that the item's reference count is greater than 0.
    ///
    /// # Safety
    ///
    /// See the type-level safety model.
    pub unsafe fn use_(&self, base: impl Base, id: Id) {
        assert!(id > Id::ZERO);
        assert!(id.to_usize() < self.layout.cap);

        unsafe {
            let item = self.item(base.base_ptr(), id);

            // If `use` is being called on an item with 0 references, then
            // either someone forgot to call it before, released too early
            // or lied about releasing. In any case something is wrong and
            // shouldn't be allowed.
            debug_assert!((*item).meta.r#ref > R::ZERO);

            (*item).meta.r#ref += R::ONE;
        }
    }

    /// Increment an item's reference count by a specified number.
    ///
    /// Asserts that the item's reference count is greater than 0.
    ///
    /// # Safety
    ///
    /// See the type-level safety model.
    pub unsafe fn use_multiple(&self, base: impl Base, id: Id, n: R) {
        assert!(id > Id::ZERO);
        assert!(id.to_usize() < self.layout.cap);

        unsafe {
            let item = self.item(base.base_ptr(), id);

            // If `use` is being called on an item with 0 references, then
            // either someone forgot to call it before, released too early
            // or lied about releasing. In any case something is wrong and
            // shouldn't be allowed.
            debug_assert!((*item).meta.r#ref > R::ZERO);

            (*item).meta.r#ref += n;
        }
    }

    /// Get an item by its ID without incrementing its reference count.
    ///
    /// Asserts that the item's reference count is greater than 0.
    ///
    /// # Safety
    ///
    /// See the type-level safety model.
    pub unsafe fn get(&self, base: impl Base, id: Id) -> *mut T {
        assert!(id > Id::ZERO);
        assert!(id.to_usize() < self.layout.cap);

        unsafe {
            let item = self.item(base.base_ptr(), id);

            debug_assert!((*item).meta.r#ref > R::ZERO);

            core::ptr::addr_of_mut!((*item).value).cast::<T>()
        }
    }

    /// Releases a reference to an item by its ID.
    ///
    /// Asserts that the item's reference count is greater than 0.
    ///
    /// # Safety
    ///
    /// See the type-level safety model.
    pub unsafe fn release(&mut self, base: impl Base, id: Id) {
        assert!(id > Id::ZERO);
        assert!(id.to_usize() < self.layout.cap);

        unsafe {
            let item = self.item(base.base_ptr(), id);

            assert!((*item).meta.r#ref > R::ZERO);
            (*item).meta.r#ref -= R::ONE;
            if (*item).meta.r#ref == R::ZERO {
                self.living -= 1;
            }
        }
    }

    /// Release a specified number of references to an item by its ID.
    ///
    /// Asserts that the item's reference count is at least `n`.
    ///
    /// # Safety
    ///
    /// See the type-level safety model.
    pub unsafe fn release_multiple(&mut self, base: impl Base, id: Id, n: Id) {
        assert!(id > Id::ZERO);
        assert!(id.to_usize() < self.layout.cap);

        unsafe {
            let item = self.item(base.base_ptr(), id);

            let n = R::from_usize(n.to_usize());
            assert!((*item).meta.r#ref >= n);
            (*item).meta.r#ref -= n;

            if (*item).meta.r#ref == R::ZERO {
                self.living -= 1;
            }
        }
    }

    /// Get the ref count for an item by its ID.
    ///
    /// # Safety
    ///
    /// See the type-level safety model.
    pub unsafe fn ref_count(&self, base: impl Base, id: Id) -> R {
        assert!(id > Id::ZERO);
        assert!(id.to_usize() < self.layout.cap);

        unsafe { (*self.item(base.base_ptr(), id)).meta.r#ref }
    }

    /// Get the current number of non-dead items in the set.
    pub fn count(&self) -> usize {
        self.living
    }

    /// Return an iterator over the live entries in this set.
    ///
    /// # Safety
    ///
    /// See the type-level safety model; additionally the set must not be
    /// mutated (and the buffer must stay valid) while the iterator is used.
    pub unsafe fn iterator(&self, base: impl Base) -> SetIterator<T, Id, R> {
        SetIterator {
            items: self.items.ptr(base),
            id: Id::ONE,
            end: self.next_id,
        }
    }

    /// Delete an item, removing any references from
    /// the table, and freeing its ID to be reused.
    unsafe fn delete_item(&mut self, base: *mut u8, id: Id, ctx: &Ctx) {
        unsafe {
            let item = *self.item(base, id);

            if item.meta.bucket.to_usize() > self.layout.table_cap {
                return;
            }

            debug_assert!(*self.tbl(base, item.meta.bucket.to_usize()) == id);

            // Inform the context struct that we're
            // deleting the dead item's value for good.
            ctx.deleted(item.value.assume_init_ref());

            self.psl_stats[item.meta.psl.to_usize()] -= Id::ONE;
            *self.tbl(base, item.meta.bucket.to_usize()) = Id::ZERO;
            *self.item(base, id) = Item::default();

            let mask = self.layout.table_mask;
            let mut p: Id = item.meta.bucket;
            let mut n: Id = Id::from_usize(p.to_usize().wrapping_add(1) & mask.to_usize());

            loop {
                let tn = *self.tbl(base, n.to_usize());
                if tn == Id::ZERO {
                    break;
                }
                let it = self.item(base, tn);
                if (*it).meta.psl == Id::ZERO {
                    break;
                }
                (*it).meta.bucket = p;
                self.psl_stats[(*it).meta.psl.to_usize()] -= Id::ONE;
                (*it).meta.psl -= Id::ONE;
                self.psl_stats[(*it).meta.psl.to_usize()] += Id::ONE;
                *self.tbl(base, p.to_usize()) = tn;
                p = n;
                n = Id::from_usize(p.to_usize().wrapping_add(1) & mask.to_usize());
            }

            while self.max_psl > Id::ZERO && self.psl_stats[self.max_psl.to_usize()] == Id::ZERO {
                self.max_psl -= Id::ONE;
            }

            *self.tbl(base, p.to_usize()) = Id::ZERO;

            self.assert_integrity(base, ctx);
        }
    }

    /// Find an item in the table and return its ID.
    /// If the item does not exist in the table, null is returned.
    ///
    /// # Safety
    ///
    /// See the type-level safety model.
    pub unsafe fn lookup(&self, base: impl Base, value: T) -> Option<Id> {
        // SAFETY: forwarded.
        unsafe { self.lookup_context(base, value, &self.context) }
    }

    /// # Safety
    ///
    /// See the type-level safety model.
    pub unsafe fn lookup_context(&self, base: impl Base, value: T, ctx: &Ctx) -> Option<Id> {
        // A zero-capacity set (a valid special case of Layout.init)
        // contains nothing and has a zero-size table, so we can't
        // probe it: table[0] would read whatever memory follows the
        // set in the backing buffer.
        if self.layout.table_cap == 0 {
            return None;
        }

        let base = base.base_ptr();
        let hash: u64 = ctx.hash(&value);
        let mask = self.layout.table_mask.to_usize() as u64;

        for i in 0..self.max_psl.to_usize() + 1 {
            let p: usize = (hash.wrapping_add(i as u64) & mask) as usize;
            // SAFETY: p is masked into the table; ids in the table are valid.
            unsafe {
                let id = *self.tbl(base, p);

                // Empty bucket, our item cannot have probed to
                // any point after this, meaning it's not present.
                if id == Id::ZERO {
                    return None;
                }

                let item = self.item(base, id);

                // An item with a shorter probe sequence length would never
                // end up in the middle of another sequence, since it would
                // be swapped out if inserted before the new sequence, and
                // would not be swapped in if inserted afterwards.
                //
                // As such, our item cannot be present.
                if (*item).meta.psl.to_usize() < i {
                    return None;
                }

                // If the item is a part of the same probe sequence,
                // we make sure it's not dead and then check to see
                // if it matches the value we're looking for.
                if (*item).meta.psl.to_usize() == i
                    && (*item).meta.r#ref > R::ZERO
                    && ctx.eql(&value, (*item).value.assume_init_ref())
                {
                    return Some(id);
                }
            }
        }

        None
    }

    /// Insert the given value into the hash table with the given ID.
    ///
    /// If runtime safety is enabled, asserts that
    /// the value is not already present in the table.
    unsafe fn insert(&mut self, base: *mut u8, value: T, new_id: Id, ctx: &Ctx) -> Id {
        debug_assert!(unsafe { self.lookup_context(base, value, ctx) }.is_none());

        // The new item that we'll put in to the table.
        let mut new_item: Item<T, Id, R> = Item {
            value: MaybeUninit::new(value),
            meta: ItemMetadata {
                bucket: Id::MAX,
                psl: Id::ZERO,
                r#ref: R::ZERO,
            },
        };

        let hash: u64 = ctx.hash(&value);
        let mask = self.layout.table_mask.to_usize() as u64;

        let mut held_id: Id = new_id;
        // SAFETY: `new_item` is only accessed through this pointer until
        // the loop below finishes.
        let mut held_item: *mut Item<T, Id, R> = core::ptr::addr_of_mut!(new_item);

        let mut chosen_id: Id = new_id;

        unsafe {
            for i in 0..self.layout.table_cap - 1 {
                let p: Id = Id::from_usize((hash.wrapping_add(i as u64) & mask) as usize);
                let id = *self.tbl(base, p.to_usize());

                // Empty bucket, put our held item in to it and break.
                if id == Id::ZERO {
                    *self.tbl(base, p.to_usize()) = held_id;
                    (*held_item).meta.bucket = p;
                    self.psl_stats[(*held_item).meta.psl.to_usize()] += Id::ONE;
                    self.max_psl = self.max_psl.max((*held_item).meta.psl);
                    break;
                }

                let item = self.item(base, id);

                // If there's a dead item then we resurrect it
                // for our value so that we can reuse its ID,
                // unless its ID is greater than the one we're
                // given (i.e. prefer smaller IDs).
                if (*item).meta.r#ref == R::ZERO {
                    // Dead items aren't super common relative
                    // to other places to insert/swap the held
                    // item in to the set.

                    // Inform the context struct that we're
                    // deleting the dead item's value for good.
                    ctx.deleted((*item).value.assume_init_ref());

                    // Reap the dead item.
                    self.psl_stats[(*item).meta.psl.to_usize()] -= Id::ONE;
                    *item = Item::default();

                    // Only resurrect this item if it has a
                    // smaller id than the one we were given.
                    if id < new_id {
                        chosen_id = id;
                    }

                    // Put the currently held item in to the
                    // bucket of the item that we just reaped.
                    *self.tbl(base, p.to_usize()) = held_id;
                    (*held_item).meta.bucket = p;
                    self.psl_stats[(*held_item).meta.psl.to_usize()] += Id::ONE;
                    self.max_psl = self.max_psl.max((*held_item).meta.psl);

                    break;
                }

                // If this item has a lower PSL, or has equal PSL and lower ref
                // count, then we swap it out with our held item. By doing this,
                // items with high reference counts are prioritized for earlier
                // placement. The assumption is that an item which has a higher
                // reference count will be accessed more frequently, so we want
                // to minimize the time it takes to find it.
                if (*item).meta.psl < (*held_item).meta.psl
                    || ((*item).meta.psl == (*held_item).meta.psl && (*item).meta.r#ref < (*held_item).meta.r#ref)
                {
                    // Put our held item in the bucket.
                    *self.tbl(base, p.to_usize()) = held_id;
                    (*held_item).meta.bucket = p;
                    self.psl_stats[(*held_item).meta.psl.to_usize()] += Id::ONE;
                    self.max_psl = self.max_psl.max((*held_item).meta.psl);

                    // Pick up the item that has a lower PSL.
                    held_id = id;
                    held_item = item;
                    self.psl_stats[(*item).meta.psl.to_usize()] -= Id::ONE;
                }

                // Advance to the next probe position for our held item.
                (*held_item).meta.psl += Id::ONE;
            }

            // Our chosen ID may have changed if we decided
            // to reuse a dead item's ID, so we make sure
            // the chosen bucket contains the correct ID.
            *self.tbl(base, new_item.meta.bucket.to_usize()) = chosen_id;

            // Finally place our new item in to our array.
            *self.item(base, chosen_id) = new_item;

            self.assert_integrity(base, ctx);
        }

        chosen_id
    }

    unsafe fn assert_integrity(&self, base: *mut u8, ctx: &Ctx) {
        // Disabled because this is excessively slow, only enable
        // if debugging a RefCountedSet issue or modifying its logic.
        const ENABLED: bool = false;
        if ENABLED && cfg!(debug_assertions) {
            unsafe {
                let mut psl_stats: [Id; 32] = [Id::ZERO; 32];

                // Start at item 1 because item 0 is reserved and never
                // assigned to. Its metadata doesn't matter.
                for id in 1..self.next_id.to_usize() {
                    let item = *self.item(base, Id::from_usize(id));
                    if item.meta.bucket < Id::MAX {
                        assert!((*self.tbl(base, item.meta.bucket.to_usize())).to_usize() == id);
                        psl_stats[item.meta.psl.to_usize()] += Id::ONE;
                    }
                }

                assert_eq!(psl_stats, self.psl_stats);

                psl_stats = [Id::ZERO; 32];

                for bucket in 0..self.layout.table_cap {
                    let id = *self.tbl(base, bucket);
                    if id == Id::ZERO {
                        continue;
                    }
                    let item = *self.item(base, id);
                    if item.meta.bucket < Id::MAX {
                        assert!(item.meta.bucket.to_usize() == bucket);

                        let hash: u64 = ctx.hash(item.value.assume_init_ref());
                        let p: usize = (hash.wrapping_add(item.meta.psl.to_usize() as u64)
                            & self.layout.table_mask.to_usize() as u64)
                            as usize;
                        assert!(p == bucket);

                        psl_stats[item.meta.psl.to_usize()] += Id::ONE;
                    }
                }

                assert_eq!(psl_stats, self.psl_stats);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::size::AlignedBuf;

    /// Zig `std.hash.int` for `u32` (lowbias32).
    fn hash_int(input: u32) -> u32 {
        let mut x = input;
        x ^= x >> 16;
        x = x.wrapping_mul(0x7feb352d);
        x ^= x >> 15;
        x = x.wrapping_mul(0x846ca68b);
        x ^= x >> 16;
        x
    }

    #[derive(Default, Clone, Copy)]
    struct Ctx;

    impl RefCountedSetContext<u32> for Ctx {
        fn hash(&self, value: &u32) -> u64 {
            hash_int(*value) as u64
        }

        fn eql(&self, a: &u32, b: &u32) -> bool {
            a == b
        }
    }

    type TestSet = RefCountedSet<u32, u16, u16, Ctx>;

    fn new_set(cap: usize) -> (AlignedBuf, TestSet) {
        let layout = TestSet::layout(cap);
        let buf = AlignedBuf::new(layout.total_size, TestSet::BASE_ALIGN);
        // SAFETY: buf is total_size bytes aligned to BASE_ALIGN.
        let set = unsafe { TestSet::init(OffsetBuf::init(buf.ptr()), layout, Ctx) };
        (buf, set)
    }

    // The integer item capacity against Zig's float formula.
    #[test]
    fn items_cap_matches_load_factor_float() {
        for k in 0..40 {
            let table_cap = 1usize << k;
            assert_eq!(
                TestSet::items_cap(table_cap),
                (TestSet::LOAD_FACTOR * table_cap as f64) as usize
            );
        }
        for cap in 1..=65536usize {
            let table_cap = cap.next_power_of_two();
            assert_eq!(TestSet::layout(cap).cap, (TestSet::LOAD_FACTOR * table_cap as f64) as usize);
        }
    }

    #[test]
    fn add_with_id_dead_id_resolving_to_an_existing_value() {
        let (buf, mut set) = new_set(8);
        let buf = buf.ptr();

        unsafe {
            // Create a dead item between two live ones so that it can't
            // be reaped by the trim loop in `add`, then release it. This
            // mirrors a page style set after a styled run is erased.
            let live = set.add(buf, 11).unwrap();
            let released = set.add(buf, 22).unwrap();
            let last = set.add(buf, 33).unwrap();
            set.release(buf, released);
            assert_eq!(2usize, set.count());

            // Request the dead ID for a value that is already live under
            // a different ID: we must resolve to the existing item and
            // take a reference, without changing the living count.
            let resolved = set.add_with_id(buf, 11, released).unwrap().unwrap();
            assert_eq!(live, resolved);
            assert_eq!(2u16, set.ref_count(buf, live));
            assert_eq!(2usize, set.count());

            // The living count must agree with the iterator.
            let iterated = set.iterator(buf).count();
            assert_eq!(set.count(), iterated);

            // The dead slot is still reusable for a value that is not
            // in the set: the requested ID must be used (null return).
            assert_eq!(None, set.add_with_id(buf, 44, released).unwrap());
            assert_eq!(1u16, set.ref_count(buf, released));
            assert_eq!(3usize, set.count());

            let _ = last;
        }
    }

    #[test]
    fn iterator_visits_live_entries_in_id_order() {
        let (buf, mut set) = new_set(8);
        let buf = buf.ptr();

        unsafe {
            let first = set.add(buf, 11).unwrap();
            let released = set.add(buf, 22).unwrap();
            let last = set.add(buf, 33).unwrap();
            let _ = set.add(buf, 11).unwrap();
            set.release(buf, released);

            let mut it = set.iterator(buf);

            let first_entry = it.next().unwrap();
            assert_eq!(first, first_entry.id);
            assert_eq!(11u32, *first_entry.value_ptr);

            let last_entry = it.next().unwrap();
            assert_eq!(last, last_entry.id);
            assert_eq!(33u32, *last_entry.value_ptr);

            assert!(it.next().is_none());
        }
    }
}
