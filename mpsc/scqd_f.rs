//! A concurrent, lock-free, FIFO queue.
//! Utilizes SCQD-F (Scalable Circular Queue with Distributed Free-list) Architecture.
//!
//! The SCQD-F queue uses three lock-free rings:
//! - `aq`: Allocated Queue (indices of enqueued messages)
//! - `fq1`: Free Queue 1 (block IDs for free index blocks)
//! - `fq2`: Free Queue 2 (block IDs for allocated index blocks)

use crate::sync::mpsc2::lfring::LFRing;

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::sync::Arc;

/// Configuration for SCQD-F queue
/// Maybe change order based on channel size, leave 18 for unbounded (assuming architectural limits?)
const SCQD_ORDER: usize = 18;              // Log2 of total queue capacity
const SCQD_INDEX_BLOCK_SIZE: usize = 16;   // Indices per block
const SCQD_CACHE_SIZE: usize = 32;         // Per-thread cache size
const MIN_CACHE_SIZE: usize = SCQD_INDEX_BLOCK_SIZE;  // When to fetch new block

/// Total number of message slots
const TOTAL_SLOTS: usize = 1 << SCQD_ORDER;

/// Number of index blocks
const NUM_BLOCKS: usize = 1 << (SCQD_ORDER - 4);  // 2^14

/// Shared SCQD-F queue data structure.
pub(crate) struct Queue<T> {
    /// Allocated Queue: ring of enqueued index slots
    aq: Box<LFRing>,
    /// Free Queue 1: ring of free index block IDs (initially full)
    fq1: Box<LFRing>,
    /// Free Queue 2: ring of allocated index block IDs
    fq2: Box<LFRing>,
    /// Index array: flat array of 2^18 indices for mapping
    /// Organized as blocks: idx[block_id * 16 .. block_id * 16 + 16]
    idx: Box<[usize]>,
    /// Value array: flat allocation holding T values
    val: *mut T,
}

/// Handle for thread-local operation (per sender/receiver).
struct Handle {
    /// Local cache of free indices
    owned_cache: Box<[usize]>,
    /// Number of valid indices in owned_cache
    local_size: usize,
}

/// Transmit handle for SCQD-F queue.
pub(crate) struct Tx<T> {
    /// Shared queue data
    queue: Arc<Queue<T>>,
    /// Thread-local handle
    handle: Handle,
}

/// Receive handle for SCQD-F queue.
pub(crate) struct Rx<T> {
    /// Shared queue data
    queue: Arc<Queue<T>>,
    /// Thread-local handle
    handle: Handle,
}

/// Result of `Rx::try_recv`.
pub(crate) enum TryDequeueResult<T> {
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
    
    // Check this implementation, especially with both flat index array and data cache being allocated
    fn new() -> Self {
        unsafe {
            // Initialize the three rings
            let aq = LFRing::new_empty(SCQD_ORDER);
            let fq1 = LFRing::new_full(SCQD_ORDER - 2);
            let fq2 = LFRing::new_empty(SCQD_ORDER - 2);

            // Initialize flat index array (identity mapping)
            let idx: Box<[usize]> = (0..TOTAL_SLOTS)
                .map(|i| i)
                .collect::<Vec<_>>()
                .into_boxed_slice();

            // Allocate data cache
            let layout = Layout::array::<T>(TOTAL_SLOTS)
                .expect("SCQD-F value array layout too large");
            let val = alloc_zeroed(layout) as *mut T;

            if val.is_null() {
                std::alloc::handle_alloc_error(layout);
                // ! Error handling?
            }

            Queue { aq, fq1, fq2, idx, val }
        }
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        unsafe {
            if !self.val.is_null() {
                let layout = Layout::array::<T>(TOTAL_SLOTS)
                    .expect("SCQD-F value array layout too large");
                dealloc(self.val as *mut u8, layout);
            }
        }
    }
}

/// Creates a new SCQD-F channel, returning a transmitter and receiver pair.
pub(crate) fn channel<T>() -> (Tx<T>, Rx<T>) {
    
    // Initialize a shared SCQD-F struct
    let queue = Arc::new(Queue::new());         

    // Multiple-Producer Transmit Channel
    let tx = Tx {
        queue: Arc::clone(&queue),              // Does each producer need an Arc clone of the queues?
        handle: Handle::new(),
    };

    // Single-Consumer Receive Channel
    let rx = Rx {
        queue,
        handle: Handle::new(),
    };

    (tx, rx)
}

impl<T> Tx<T> {

    /// Enqueue data to SCQD-F
    // pub(crate) fn enqueue() {
            // calls try_enqueue at this point
            // Is this necessary?
    // }

    /// Attempt to push a value to the cache by enqueuing a free data index.
    ///
    /// Returns `Ok(())` if the value was enqueued, or `Err(val)` if the queue
    /// is at capacity (no free indices available).
    pub(crate) fn try_enqueue(&mut self, val: T) -> Result<(), T> {

        // Attempt to grab an index, either from local cache or from fbiq
        let idx = if self.handle.local_size > 0 {
            // Get index from local cache
            self.handle.local_size -= 1;                                    // Confirm that this is supposed to decrement
            self.handle.owned_cache[self.handle.local_size]
        } else {
            // Get an index block pointer by dequeuing from fbiq
            let bidx = self.queue.fq1.dequeue(SCQD_ORDER - 2, false);
            if bidx == LFRING_EMPTY {                                       // LFRING_EMPTY == usize::MAX
                // Throw an error with the value where enqueue was attempted.
                return Err(val);        
            }


            // Is there a cleaner way to do this?
            // Copy 16 indices from the block into the local cache
            let block_start = bidx << 4;  // bidx * 16
            for i in 0..16 {
                self.handle.owned_cache[i] = self.queue.idx[block_start + i];
            }

            // Indicate the index block as allocated by enqueuing it to abiq
            let enqueue_ok = self.queue.fq2.enqueue(SCQD_ORDER - 2, bidx, false);
            if !enqueue_ok {
                // Should not happen; fq2 should have space
                return Err(val);
            }

            self.handle.local_size = 16;
            self.handle.local_size -= 1;
            self.handle.owned_cache[self.handle.local_size]
        };

        // Store the value in the val array using MaybeUninit to ensure proper cleanup
        unsafe {
            std::ptr::write(self.queue.val.add(idx), val);
        }

        // Enqueue the index to aq
        let enqueue_ok = self.queue.aq.enqueue(SCQD_ORDER, idx, false);

        // TODO: Code will not reach this point since lfring.enqueue() does not have a way to return false.
        //       Also, what will happen to the allocated index block in abiq? See below comments. 
        if !enqueue_ok {
            // Enqueue failed; drop the value we just wrote and return the index to cache
            unsafe {
                let recovered_val = std::ptr::read(self.queue.val.add(idx));
                self.handle.owned_cache[self.handle.local_size] = idx;
                self.handle.local_size += 1;
                return Err(recovered_val);

                // Also account for if an index was enqueued into abiq.
                // How will we remove the index block from abiq if we allocated one index from it?
            }
        }

        Ok(())      
    }

    /// Set TX_CLOSED, but where?
    pub(crate) fn close(&self) {
        // create a flag in the channel?
    }
}

/// Add fmt::Debug for Tx<T>?

impl<T> Rx<T> {

    /// TODO: Returns whether or not SCQD-F is empty
    ///     
    // pub(crate) fn is_empty(&self, tx: &Tx<T>) -> bool {

    // }

    /// TODO: Returns the number of data values in SCQD-F
    ///     
    // pub(crate) fn len() {

    // }


    /// TODO: Dequeue function with error handler
    // pub(crate) fn dequeue() {
        // calls try_dequeue()
    //}

    /// Attempt to return a value by dequeuing a data index.
    ///
    /// Returns `Ok(T)` if a value was dequeued, or `None` if the queue is empty.
    pub(crate) fn try_dequeue(&mut self) -> Option<T> {
        // Try to dequeue an index from the allocated queue
        let idx = self.queue.aq.dequeue(SCQD_ORDER, false);
        if idx == usize::MAX {
            return None;  // Queue is empty
        }

        // Read the value from the val array
        let val = unsafe { std::ptr::read(self.queue.val.add(idx)) };

        // Add the index to our local cache
        self.handle.owned_cache[self.handle.local_size] = idx;
        self.handle.local_size += 1;

        // If local cache is full (32 indices), return half to the free queue
        if self.handle.local_size == SCQD_CACHE_SIZE {
            // Dequeue a block ID from fq2
            let bidx = self.queue.fq2.dequeue(SCQD_ORDER - 2, false);
            if bidx != usize::MAX {
                // Copy the upper 16 indices back into the idx array
                let block_start = bidx << 4;
                for i in 0..SCQD_INDEX_BLOCK_SIZE {
                    self.queue.idx[block_start + i] = self.handle.owned_cache[SCQD_INDEX_BLOCK_SIZE + i];
                }

                // Return the upper half's local_size is now 16
                self.handle.local_size = SCQD_INDEX_BLOCK_SIZE;

                // Enqueue the block back to fq1 (free queue)
                let _ = self.queue.fq1.enqueue(SCQD_ORDER - 2, bidx, false);
            }
        }

        Some(val)
    }

    /// TODO: free() function after closing Transmit channels. 
    
}

/// Add fmt::Debug for Rx<T>?


