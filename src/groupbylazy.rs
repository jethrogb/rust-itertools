use std::cell::{Cell, RefCell};
use std::vec;

/// A trait to unify FnMut for GroupByLazy with the chunk key in ChunksLazy
trait KeyFunction<A> {
    type Key;
    fn call_mut(&mut self, arg: A) -> Self::Key;
}

impl<'a, A, K, F: ?Sized> KeyFunction<A> for F
    where F: FnMut(A) -> K
{
    type Key = K;
    #[inline]
    fn call_mut(&mut self, arg: A) -> Self::Key {
        (*self)(arg)
    }
}


/// ChunkIndex acts like the grouping key function for ChunksLazy
struct ChunkIndex {
    size: usize,
    index: usize,
    key: usize,
}

impl ChunkIndex {
    #[inline(always)]
    fn new(size: usize) -> Self {
        ChunkIndex {
            size: size,
            index: 0,
            key: 0,
        }
    }
}

impl<'a, A> KeyFunction<A> for ChunkIndex {
    type Key = usize;
    #[inline(always)]
    fn call_mut(&mut self, _arg: A) -> Self::Key {
        if self.index == self.size {
            self.key += 1;
            self.index = 0;
        }
        self.index += 1;
        self.key
    }
}


struct GroupInner<K, I, F>
    where I: Iterator
{
    key: F,
    iter: I,
    current_key: Option<K>,
    current_elt: Option<I::Item>,
    /// flag set if iterator is exhausted
    done: bool,
    /// Index of group we are currently buffering or visiting
    top: usize,
    /// Least index for which we still have elements buffered
    bot: usize,
    /// Group index for `buffer[0]` -- the slots bufbot..bot are unused
    /// and will be erased when that range is large enough.
    bufbot: usize,
    /// Buffered groups, from `bufbot` (index 0) to `top`.
    buffer: Vec<vec::IntoIter<I::Item>>,
    /// index of last group iter that was dropped, usize::MAX == none
    dropped_group: usize,
}

impl<K, I, F> GroupInner<K, I, F>
    where I: Iterator,
          F: for<'a> KeyFunction<&'a I::Item, Key=K>,
          K: PartialEq,
{
    /// `client`: Index of group that requests next element
    #[inline(always)]
    fn step(&mut self, client: usize) -> Option<I::Item> {
        /*
        println!("client={}, bufbot={}, bot={}, top={}, buffers=[{}]",
                 client, self.bufbot, self.bot, self.top,
                 self.buffer.iter().format(", ", |elt, f| f(&elt.len())));
         */
        if client < self.bot {
            None
        } else if client < self.top ||
            (client == self.top && self.buffer.len() > self.top - self.bufbot)
        {
            self.lookup_buffer(client)
        } else if self.done {
            None
        } else if self.top == client {
            self.step_current()
        } else {
            self.step_buffering(client)
        }
    }

    #[inline(never)]
    fn lookup_buffer(&mut self, client: usize) -> Option<I::Item> {
        // if `bufidx` doesn't exist in self.buffer, it might be empty
        let bufidx = client - self.bufbot;
        if client < self.bot {
            return None;
        }
        let elt = self.buffer.get_mut(bufidx).and_then(|queue| queue.next());
        if elt.is_none() && client == self.bot {
            // FIXME: VecDeque is unfortunately not zero allocation when empty,
            // so we do this job manually.
            // `bufbot..bot` is unused, and if it's large enough, erase it.
            self.bot += 1;
            // skip forward further empty queues too
            while self.buffer.get(self.bot - self.bufbot)
                             .map_or(false, |buf| buf.len() == 0)
            {
                self.bot += 1;
            }

            let nclear = self.bot - self.bufbot;
            if nclear > 0 && nclear >= self.buffer.len() / 2 {
                let mut i = 0;
                self.buffer.retain(|buf| {
                    i += 1;
                    debug_assert!(buf.len() == 0 || i > nclear);
                    i > nclear
                });
                self.bufbot = self.bot;
            }
        }
        elt
    }

    /// Take the next element from the iterator, and set the done
    /// flag if exhausted. Must not be called after done.
    #[inline(always)]
    fn next_element(&mut self) -> Option<I::Item> {
        debug_assert!(!self.done);
        match self.iter.next() {
            None => { self.done = true; None }
            otherwise => otherwise,
        }
    }


    #[inline(never)]
    fn step_buffering(&mut self, client: usize) -> Option<I::Item> {
        // requested a later group -- walk through the current group up to
        // the requested group index, and buffer the elements (unless
        // the group is marked as dropped).
        // Because the `Groups` iterator is always the first to request
        // each group index, client is the next index efter top.
        debug_assert!(self.top + 1 == client);
        let mut group = Vec::new();

        if let Some(elt) = self.current_elt.take() {
            if self.top != self.dropped_group {
                group.push(elt);
            }
        }
        let mut first_elt = None; // first element of the next group

        while let Some(elt) = self.next_element() {
            let key = self.key.call_mut(&elt);
            match self.current_key.take() {
                None => {}
                Some(old_key) => if old_key != key {
                    self.current_key = Some(key);
                    first_elt = Some(elt);
                    break;
                },
            }
            self.current_key = Some(key);
            if self.top != self.dropped_group {
                group.push(elt);
            }
        }

        if self.top != self.dropped_group {
            self.push_next_group(group);
        }
        if first_elt.is_some() {
            self.top += 1;
            debug_assert!(self.top == client);
        }
        first_elt
    }

    fn push_next_group(&mut self, group: Vec<I::Item>) {
        // When we add a new buffered group, fill up slots between bot and top
        while self.top - self.bufbot > self.buffer.len() {
            if self.buffer.is_empty() {
                self.bufbot += 1;
                self.bot += 1;
            } else {
                self.buffer.push(Vec::new().into_iter());
            }
        }
        self.buffer.push(group.into_iter());
        debug_assert!(self.top + 1 - self.bufbot == self.buffer.len());
    }

    /// This is the immediate case, where we use no buffering
    #[inline]
    fn step_current(&mut self) -> Option<I::Item> {
        debug_assert!(!self.done);
        if let elt @ Some(..) = self.current_elt.take() {
            return elt;
        }
        match self.next_element() {
            None => None,
            Some(elt) => {
                let key = self.key.call_mut(&elt);
                match self.current_key.take() {
                    None => {}
                    Some(old_key) => if old_key != key {
                        self.current_key = Some(key);
                        self.current_elt = Some(elt);
                        self.top += 1;
                        return None;
                    },
                }
                self.current_key = Some(key);
                Some(elt)
            }
        }
    }

    /// Request the just started groups' key.
    ///
    /// `client`: Index of group
    ///
    /// **Panics** if no group key is available.
    fn group_key(&mut self, client: usize) -> K {
        // This can only be called after we have just returned the first
        // element of a group.
        // Perform this by simply buffering one more element, grabbing the
        // next key.
        debug_assert!(!self.done);
        debug_assert!(client == self.top);
        debug_assert!(self.current_key.is_some());
        debug_assert!(self.current_elt.is_none());
        let old_key = self.current_key.take().unwrap();
        if let Some(elt) = self.next_element() {
            let key = self.key.call_mut(&elt);
            if old_key != key {
                self.top += 1;
            }
            self.current_key = Some(key);
            self.current_elt = Some(elt);
        }
        old_key
    }
}

impl<K, I, F> GroupInner<K, I, F>
    where I: Iterator,
{
    /// Called when a group is dropped
    fn drop_group(&mut self, client: usize) {
        // It's only useful to track the maximal index
        if self.dropped_group == !0 || client > self.dropped_group {
            self.dropped_group = client;
        }
    }
}

/// `GroupByLazy` is the storage for the lazy grouping operation.
///
/// If the groups are consumed in their original order, or if each
/// group is dropped without keeping it around, then `GroupByLazy` uses
/// no allocations. It needs allocations only if several group iterators
/// are alive at the same time.
///
/// This type implements `IntoIterator` (it is **not** an iterator
/// itself), because the group iterators need to borrow from this
/// value. It should be stored in a local variable or temporary and
/// iterated.
///
/// See [`.group_by_lazy()`](trait.Itertools.html#method.group_by_lazy) for more information.
pub struct GroupByLazy<K, I, F>
    where I: Iterator,
{
    inner: RefCell<GroupInner<K, I, F>>,
    // the group iterator's current index. Keep this in the main value
    // so that simultaneous iterators all use the same state.
    index: Cell<usize>,
}

/// Create a new
pub fn new<K, J, F>(iter: J, f: F) -> GroupByLazy<K, J::IntoIter, F>
    where J: IntoIterator,
          F: FnMut(&J::Item) -> K,
{
    GroupByLazy {
        inner: RefCell::new(GroupInner {
            key: f,
            iter: iter.into_iter(),
            current_key: None,
            current_elt: None,
            done: false,
            top: 0,
            bot: 0,
            bufbot: 0,
            buffer: Vec::new(),
            dropped_group: !0,
        }),
        index: Cell::new(0),
    }
}

impl<K, I, F> GroupByLazy<K, I, F>
    where I: Iterator,
{
    /// `client`: Index of group that requests next element
    fn step(&self, client: usize) -> Option<I::Item>
        where F: FnMut(&I::Item) -> K,
              K: PartialEq,
    {
        self.inner.borrow_mut().step(client)
    }

    /// `client`: Index of group
    fn drop_group(&self, client: usize) {
        self.inner.borrow_mut().drop_group(client)
    }
}

impl<'a, K, I, F> IntoIterator for &'a GroupByLazy<K, I, F>
    where I: Iterator,
          I::Item: 'a,
          F: FnMut(&I::Item) -> K,
          K: PartialEq
{
    type Item = (K, Group<'a, K, I, F>);
    type IntoIter = Groups<'a, K, I, F>;

    fn into_iter(self) -> Self::IntoIter {
        Groups { parent: self }
    }
}


/// An iterator that yields the Group iterators.
///
/// Iterator element type is `(K, Group)`:
/// the group's key `K` and the group's iterator.
///
/// See [`.group_by_lazy()`](trait.Itertools.html#method.group_by_lazy) for more information.
pub struct Groups<'a, K: 'a, I: 'a, F: 'a>
    where I: Iterator,
          I::Item: 'a
{
    parent: &'a GroupByLazy<K, I, F>,
}

impl<'a, K, I, F> Iterator for Groups<'a, K, I, F>
    where I: Iterator,
          I::Item: 'a,
          F: FnMut(&I::Item) -> K,
          K: PartialEq
{
    type Item = (K, Group<'a, K, I, F>);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let index = self.parent.index.get();
        self.parent.index.set(index + 1);
        let inner = &mut *self.parent.inner.borrow_mut();
        inner.step(index).map(|elt| {
            let key = inner.group_key(index);
            (key, Group {
                parent: self.parent,
                index: index,
                first: Some(elt),
            })
        })
    }
}

/// An iterator for the elements in a single group.
///
/// Iterator element type is `I::Item`.
pub struct Group<'a, K: 'a, I: 'a, F: 'a>
    where I: Iterator,
          I::Item: 'a,
{
    parent: &'a GroupByLazy<K, I, F>,
    index: usize,
    first: Option<I::Item>,
}

impl<'a, K, I, F> Drop for Group<'a, K, I, F>
    where I: Iterator,
          I::Item: 'a,
{
    fn drop(&mut self) {
        self.parent.drop_group(self.index);
    }
}

impl<'a, K, I, F> Iterator for Group<'a, K, I, F>
    where I: Iterator,
          I::Item: 'a,
          F: FnMut(&I::Item) -> K,
          K: PartialEq,
{
    type Item = I::Item;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if let elt @ Some(..) = self.first.take() {
            return elt;
        }
        self.parent.step(self.index)
    }
}

///// ChunksLazy /////

/// Create a new
pub fn new_chunks<J>(iter: J, size: usize) -> ChunksLazy<J::IntoIter>
    where J: IntoIterator,
{
    ChunksLazy {
        inner: RefCell::new(GroupInner {
            key: ChunkIndex::new(size),
            iter: iter.into_iter(),
            current_key: None,
            current_elt: None,
            done: false,
            top: 0,
            bot: 0,
            bufbot: 0,
            buffer: Vec::new(),
            dropped_group: !0,
        }),
        index: Cell::new(0),
    }
}


/// `ChunkLazy` is the storage for a lazy chunking operation.
///
/// `ChunksLazy` behaves just like `GroupByLazy`: it is iterable, and
/// it only buffers if several chunk iterators are alive at the same time.
///
/// This type implements `IntoIterator` (it is **not** an iterator
/// itself), because the chunk iterators need to borrow from this
/// value. It should be stored in a local variable or temporary and
/// iterated.
///
/// Iterator element type is `Chunk`, each chunk's iterator.
///
/// See [`.chunks_lazy()`](trait.Itertools.html#method.chunks_lazy) for more information.
pub struct ChunksLazy<I>
    where I: Iterator,
{
    inner: RefCell<GroupInner<usize, I, ChunkIndex>>,
    // the chunk iterator's current index. Keep this in the main value
    // so that simultaneous iterators all use the same state.
    index: Cell<usize>,
}


impl<I> ChunksLazy<I>
    where I: Iterator,
{
    /// `client`: Index of chunk that requests next element
    fn step(&self, client: usize) -> Option<I::Item> {
        self.inner.borrow_mut().step(client)
    }

    /// `client`: Index of chunk
    fn drop_group(&self, client: usize) {
        self.inner.borrow_mut().drop_group(client)
    }
}

impl<'a, I> IntoIterator for &'a ChunksLazy<I>
    where I: Iterator,
          I::Item: 'a,
{
    type Item = Chunk<'a, I>;
    type IntoIter = Chunks<'a, I>;

    fn into_iter(self) -> Self::IntoIter {
        Chunks {
            parent: self,
        }
    }
}


/// An iterator that yields the Chunk iterators.
///
/// Iterator element type is `Chunk`.
///
/// See [`.chunks_lazy()`](trait.Itertools.html#method.chunks_lazy) for more information.
pub struct Chunks<'a, I: 'a>
    where I: Iterator,
          I::Item: 'a,
{
    parent: &'a ChunksLazy<I>,
}

impl<'a, I> Iterator for Chunks<'a, I>
    where I: Iterator,
          I::Item: 'a,
{
    type Item = Chunk<'a, I>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let index = self.parent.index.get();
        self.parent.index.set(index + 1);
        let inner = &mut *self.parent.inner.borrow_mut();
        inner.step(index).map(|elt| {
            Chunk {
                parent: self.parent,
                index: index,
                first: Some(elt),
            }
        })
    }
}

/// An iterator for the elements in a single chunk.
///
/// Iterator element type is `I::Item`.
pub struct Chunk<'a, I: 'a>
    where I: Iterator,
          I::Item: 'a,
{
    parent: &'a ChunksLazy<I>,
    index: usize,
    first: Option<I::Item>,
}

impl<'a, I> Drop for Chunk<'a, I>
    where I: Iterator,
          I::Item: 'a,
{
    fn drop(&mut self) {
        self.parent.drop_group(self.index);
    }
}

impl<'a, I> Iterator for Chunk<'a, I>
    where I: Iterator,
          I::Item: 'a,
{
    type Item = I::Item;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if let elt @ Some(..) = self.first.take() {
            return elt;
        }
        self.parent.step(self.index)
    }
}
