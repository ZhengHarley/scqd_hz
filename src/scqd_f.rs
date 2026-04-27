//! A concurrent, lock-free, FIFO queue.
//! Utilizes SCQD-F (Scalable Circular Queue with Distributed Free-list) Architecture.
//!
//! The SCQD-F queue uses three lock-free rings:
//! - `aq`: Allocated Queue (indices of enqueued messages)
//! - `fq1`: Free Queue 1 (block IDs for free index blocks - initially full)
//! - `fq2`: Free Queue 2 (block IDs that have been allocated)

// use crate::sync::mpmc::lfring::LFRing;
use crate::lfring::{LFRing};
use std::alloc::{alloc_zeroed, dealloc, Layout};
// use std::mem::MaybeUninit;
// use std::ptr::NonNull;
use std::sync::Arc;
// use std::sync::atomic::{AtomicPtr};
use std::cell::UnsafeCell;

/// Configuration constants for SCQD-F queue
const SCQD_INDEX_BLOCK_SIZE: usize = 16;   // Indices per block
const SCQD_CACHE_SIZE: usize = 32;         // Per-thread cache size
const MIN_CACHE_SIZE: usize = SCQD_INDEX_BLOCK_SIZE;  // When to fetch new block
const DEFAULT_SCQD_ORDER: usize = 18;      // Default log2 of queue capacity (2^18 = 262144)


/// Calculate SCQD_ORDER from a desired capacity by rounding up to next power of 2.
/// Returns the order (log2) needed to accommodate at least `capacity` items.
#[inline]
fn capacity_to_order(capacity: usize) -> usize {
    if capacity == 0 {
        return DEFAULT_SCQD_ORDER;
    }
    // Find the smallest power of 2 >= capacity
    let bits_needed = 64 - (capacity - 1).leading_zeros() as usize;
    bits_needed.max(4)  // Minimum order of 4 (16 slots)
}

/// Shared SCQD-F queue data structure.
pub(crate) struct Queue<T> {
    
    order: usize,       /// Log2 of queue capacity (order parameter for rings)
    aq: Box<LFRing>,    /// Allocated Queue: ring of enqueued index slots
    fq1: Box<LFRing>,   /// Free Queue 1: ring of free index block IDs (initially full)
    fq2: Box<LFRing>,   /// Free Queue 2: ring of allocated index block IDs
    idx: UnsafeCell<Box<[usize]>>,  /// Index array: flat array of 2^order indices for mapping
                        /// Organized as blocks: idx[block_id * 16 .. block_id * 16 + 16]
    val: UnsafeCell<*mut T>,        // Value array: flat allocation holding T values
}

/// Handle for thread-local operation (per sender/receiver).
/// TODO: See how this can be used for multicore functionality
///         Since this is similar to Jason's code
struct Handle {
    /// Local cache of free indices
    owned_cache: Box<[usize]>,
    /// Number of valid indices in owned_cache
    local_size: usize,
}

/// Transmit handle for SCQD-F queue.
pub(crate) struct Tx<T> {
    /// Shared queue data
    queue: Arc<Queue<T>>,               // Use Arc to count how many Senders the Queues have. Do we need it
    /// Thread-local handle
    handle: Handle,
}

/// Receive handle for SCQD-F queue.
/// 
/// Dev Notes:
/// Would we need a handle for Rx<T>? See DPDK code for reference
pub(crate) struct Rx<T> {
    /// Shared queue data
    queue: Arc<Queue<T>>,
    /// Thread-local handle
    handle: Handle,
}

/// Result of `Rx::try_recv`.
pub(crate) enum TryPopResult<T> {
    /// Successfully returned a value.
    Ok(T),
    /// The queue is empty.
    Empty,
    /// The channel is empty and closed.
    ///
    /// Returned when the send half is closed (all senders dropped).
    Closed,
    /// The channel is not empty, but the first value is being written.
    Busy,
}

impl Handle {
    /// Create a new handle with empty cache.
    fn new() -> Self {
        Handle {
            owned_cache: vec![0; SCQD_CACHE_SIZE].into_boxed_slice(),
            local_size: 0,
        }
    }
}

impl<T> Queue<T> {
    /// Create a new SCQD-F queue with the given order (derived from capacity).
    fn new(order: usize) -> Self {
        unsafe {
            let total_slots = 1 << order;
            eprintln!("DEBUG: Queue has size {total_slots}");

            // Initialize the three rings
            let aq = LFRing::init_empty(order);
            let fq1 = LFRing::init_full(order - 2);
            let fq2 = LFRing::init_empty(order - 2);

            // Initialize flat index array (identity mapping)
            let idx_box: Box<[usize]> = (0..total_slots)
                .map(|i| i)
                .collect::<Vec<_>>()
                .into_boxed_slice();

            let idx = UnsafeCell::new(idx_box);

            // Allocate data cache
            let layout = Layout::array::<T>(total_slots)
                .expect("SCQD-F value array layout too large");
            let val_mem = alloc_zeroed(layout) as *mut T;

            if val_mem.is_null() {
                std::alloc::handle_alloc_error(layout);
                // ! Error handling is deferred
            }

            let val = UnsafeCell::new(val_mem);

            Queue { order, aq, fq1, fq2, idx, val }
        }
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        unsafe {
            let p_val = *self.val.get();
            if !p_val.is_null() {
                let total_slots = 1 << self.order;
                let layout = Layout::array::<T>(total_slots)
                    .expect("SCQD-F value array layout too large");
                dealloc(p_val as *mut u8, layout);
            }
        }
    }
}

/// Creates a new SCQD-F channel with the specified capacity, returning a transmitter and receiver pair.
/// 
/// The `capacity` parameter is rounded up to the nearest power of 2.
/// If capacity is 0, uses the default order of 18 (262,144 slots).
/// 
/// Safety:
/// 
/// Dev Notes:
/// chan.rs won't send a capacity parameter. If bounded.rs provides a size, it will be in Semaphore.bound
/// If possible, move to remove the capacity parameter
pub(crate) fn channel<T>(capacity: usize) -> (Tx<T>, Rx<T>) {

    let order = capacity_to_order(capacity);
    
    // Initialize a shared SCQD-F struct
    let queue = Arc::new(Queue::new(order));
    eprintln!("DEBUG: Initialized Queue");

    // Multiple-Producer Transmit Channel
    let tx = Tx {
        queue: Arc::clone(&queue),
        handle: Handle::new(),
    };
    eprintln!("DEBUG: Initialized Tx");

    // Single-Consumer Receive Channel
    let rx = Rx {
        queue,                      // May need to perform a clone for this
        handle: Handle::new(),
    };
    eprintln!("DEBUG: Initialized Rx");

    (tx, rx)
}

impl<T> Tx<T> {

    /// Enqueue a value into the queue.
    ///
    /// TODO: Decide on behavior - panic on failure, block, or return Result?
    /// For now, returns Result<(), T>.
    // pub(crate) fn try_push(&mut self, val: T) -> Result<(), T> {
    //     self.push(val)
    // }

    /// Attempt to push a value to the cache by enqueuing a free data index.
    ///
    /// Returns `Ok(())` if the value was enqueued, or `Err(val)` if the queue is full
    pub(crate) fn push(&mut self, val: T) -> Result<(), T> {

        eprintln!("DEBUG: Pushing a value");

        // Attempt to get an index from local cache
        let idx = if self.handle.local_size > 0 {
            self.handle.local_size -= 1;                                    
            self.handle.owned_cache[self.handle.local_size]
        } else {

            // Get an index block pointer by dequeuing from fq1
            let bidx = self.queue.fq1.dequeue(self.queue.order - 2, false);
            if bidx == usize::MAX {
                eprintln!("ERROR: No more bidx in fq1, Queue is empty");
                return Err(val);            
            }

            let block_start = bidx << 4;  
            for i in 0..16 {
                unsafe { self.handle.owned_cache[i] = (*self.queue.idx.get())[block_start + i] };
            }

            // Indicate the index block as allocated by enqueuing it to abiq
            let enqueue_ok = self.queue.fq2.enqueue(self.queue.order - 2, bidx, false);
            if !enqueue_ok {
                // Should not happen; fq2 should have space
                eprintln!("ERROR: bidx cannot be enqueued into fq2");
                return Err(val);
            }

            self.handle.local_size = 16;
            self.handle.local_size -= 1;
            self.handle.owned_cache[self.handle.local_size]
        };

        // Store the value in the val array using MaybeUninit to ensure proper cleanup
        unsafe {
            let p_val = *self.queue.val.get();
            std::ptr::write(p_val.add(idx), val);
        }

        // Enqueue the index to aq
        let enqueue_ok = self.queue.aq.enqueue(self.queue.order, idx, false);

        // TODO: Code will not reach this point since lfring.enqueue() does not have a way to return false.
        //       Also, what will happen to the allocated index block in abiq? See below comments. 
        if !enqueue_ok {
            // Enqueue failed; drop the value we just wrote and return the index to cache

            eprintln!("ERROR: idx cannot be enqueued into aq");
            unsafe {
                let p_val = *self.queue.val.get();
                let recovered_val = std::ptr::read(p_val.add(idx));
                self.handle.owned_cache[self.handle.local_size] = idx;
                self.handle.local_size += 1;
                return Err(recovered_val);

                // Also account for if an index was enqueued into abiq.
                // How will we remove the index block from abiq if we allocated one index from it?
            }
        }

        Ok(())      
    }

    // /// Pushes a TX_CLOSED flag to SCQD-F
    // /// 
    // /// Safety: Consider the case that the Queue is full when sending this flag.
    // /// 
    // /// Development:
    // /// In a Block Linked-list implementation, each block had a map of ready bits, and tx_close would set the bit of block_size+1.
    // /// Here, we are using a less elegant solution. We wouldn't need ready bits, so we just push this value
    // /// Beware of how this affects the operation of the code in chan.rs
    // pub(crate) fn close(&self) {
    //     self.try_push(TX_CLOSED);
    // }
}



/// Add fmt::Debug for Tx<T>?




impl<T> Rx<T> {

    /// TODO: Returns whether or not SCQD-F is empty
    /// 
    /// Dev Notes:
    /// How to get the state of the queues?
    // pub(crate) fn is_empty(&self, tx: &Tx<T>) -> bool {
    //     let idx = self.queue.aq.threshold.load(Acquire);
    // }

    /// TODO: Returns the number of data values in SCQD-F
    ///     
    // pub(crate) fn len(&self, tx: &Tx<T>) -> usize {

    // }


    /// Dequeue a value from RteRing
    /// 
    /// Returns `Some(T)` if a value was dequeued, or `None` if the queue is empty.
    /// 
    pub(crate) fn pop(&mut self) -> Option<T> {
        
        eprint!("DEBUG: Popping a value");

        // Try to dequeue an index from the allocated queue
        let idx = self.queue.aq.dequeue(self.queue.order, false);
        if idx == usize::MAX {
            eprintln!("ERROR: No value in aq, queue is empty");
            return None;    // Queue is empty
        }

        // Get value from the returned index
        let val = unsafe { 
            let p_val = *self.queue.val.get();
            std::ptr::read(p_val.add(idx)) };

        // Add the index to our local cache
        self.handle.owned_cache[self.handle.local_size] = idx;
        self.handle.local_size += 1;

        // If local cache is full (32 indices), return half to the free queue
        if self.handle.local_size == SCQD_CACHE_SIZE {
            // Dequeue a block ID from fq2
            let bidx = self.queue.fq2.dequeue(self.queue.order - 2, false);
            if bidx != usize::MAX {
                // Copy the upper 16 indices back into the idx array
                let block_start = bidx << 4;
                for i in 0..SCQD_INDEX_BLOCK_SIZE {

                    unsafe{
                        (*self.queue.idx.get())[block_start + i] = self.handle.owned_cache[SCQD_INDEX_BLOCK_SIZE + i];
                    }
                    // self.queue.idx[block_start + i] = self.handle.owned_cache[SCQD_INDEX_BLOCK_SIZE + i];
                }

                // Reset local_size to 16 (lower half remains)
                self.handle.local_size = SCQD_INDEX_BLOCK_SIZE;

                // Enqueue the block back to fq1 (free queue)
                let _ = self.queue.fq1.enqueue(self.queue.order - 2, bidx, false);
            }
        }

        Some(val)
    }

    // /// Attempt to return a value by dequeuing a data index.
    // ///
    // /// Dev Notes:
    // /// If tx isn't necessary, considering taking it out of the parameter
    // pub(crate) fn try_pop(&mut self, tx: &Tx<T>) -> TryPopResult<T> {

    //     let result = self.pop(tx);

    //     match result {
            
    //     }
    // }

    // /// Free the allocations for RteRing, so all LFQueues, Handles, and other stuff
    // pub(super) unsafe fn free(&mut self) {
    //     // Implement
    // }

}


