/// NOTES FOR IMPLEMENTATION:
/// 
/// Consider creating a counter class for LFRing's head and tail
/// This way, it can either use atomic usize, or dfaa, like Jason's code


// use crate::loom::cell::UnsafeCell;
// use crate::loom::sync::atomic::{AtomicPtr, AtomicUsize};

// use std::sync::atomic::{AtomicPtr, AtomicUsize};

// use std::alloc::Layout;
// use std::mem::MaybeUninit;
// use std::ops;
// use std::ptr::{self, NonNull};
// use std::ptr::{NonNull};
use std::sync::atomic::{AtomicU64, AtomicI64};
// use std::sync::atomic::Ordering::{self, AcqRel, Acquire, Release};
use std::sync::atomic::Ordering::{AcqRel, Acquire, Release, Relaxed};


// Cache line width constants
// ! May need to be established in Cargo.toml.
const LF_CACHE_SHIFT: usize = 7;        // 2^7 = 128 bytes
const LF_CACHE_BYTES: usize = 1 << LF_CACHE_SHIFT;
const LFRING_MIN: usize = LF_CACHE_SHIFT - 3;  // For 64-bit atomics (must match config.h)

// Type casting
// type lfatomic_t = AtomicU64;
// type lfsatomic_t = AtomicI64;
// type lfatomic_big_t = AtomicU128;

// Sentinel value for empty ring
pub(crate) const LFRING_EMPTY: usize = usize::MAX;

/// A lock-free ring buffer for managing data indices
/// Stores indices that map to message slots in the SCQD-F queue.
#[repr(C)]
pub(crate) struct LFRing {
    head:       AtomicU64,             // Head pointer (u64, unsigned)
    tail:       AtomicU64,             // Tail pointer (u64, unsigned)
    threshold:  AtomicI64,             // Threshold for emptiness check (i64, signed)
    array:      Box<[AtomicU64]>,      // Heap-allocated array of data indices
}

// /// Header for the LFRing with cache-line alignment
// #[repr(C, align(64))]               // 128-byte cache line width
// struct LFRingHeader {
//     head:       AtomicU64,          // Head pointer (u64, unsigned)
//     tail:       AtomicU64,          // Tail pointer (u64, unsigned)
//     threshold:  AtomicI64,          // Threshold for emptiness check (i64, signed)
// }


/// Signed comparison helpers for cycle matching.
/// ! Make inputs as usize? Then cast the wrapping_sub() result as an isize?
#[inline]
fn lfring_cmp_lt(x: u64, y: u64) -> bool {
    ((x.wrapping_sub(y)) as i64) < 0   
}

#[inline]
fn lfring_cmp_le(x: u64, y: u64) -> bool {
    ((x.wrapping_sub(y)) as i64) <= 0
}

// #[inline]
// fn lfring_cmp_eq(x: u64, y: u64) -> bool {
//     x == y
// }

/// Maps a ring position to an array index with cache-line remapping. 
/// Assuming LFRING_MIN is not zero
#[inline]
fn raw_map(idx: u64, order: usize, n: usize) -> usize {
    let idx_u = idx as usize;
    ((idx_u & (n - 1)) >> (order - LFRING_MIN)) | ((idx_u << LFRING_MIN) & (n - 1))
}

#[inline]
fn map(idx: u64, order: usize, n: usize) -> usize {
    raw_map(idx, order + 1, n)
}

#[inline]
fn pow2(order: usize) -> usize {
    1 << order
}

#[inline]
fn threshold_value(half: usize, n: usize) -> i64 {
    (half + n - 1) as i64
}

// generate_addr_of_methods! {
//     impl LFRing {
//         unsafe fn addr_of_header(self: NonNull<Self>) -> NonNull<LFRingHeader> {
//             &self.header
//         }

//         unsafe fn addr_of_queue(self: NonNull<Self>) -> NonNull<Box<[AtomicU64]>> {
//             &self.array
//         }
//     }
// }

impl LFRing {

    /// Initializes a new LFRing with all slots empty (indices set to Usize::MAX, or -1).
    /// 
    /// # Safety
    /// 
    /// Safe if the system has enough memory to allocate the ring structure and array.
    pub(crate) fn init_empty(order: usize) -> Box<LFRing> {
        
        let n = pow2(order + 1);
        let array: Box<[AtomicU64]> = (0..n)
            .map(|_| AtomicU64::new(u64::MAX))  // -1 in unsigned representation
            .collect::<Vec<_>>()
            .into_boxed_slice();

        let lfq = LFRing {
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            threshold: AtomicI64::new(-1),
            array,
        };

        eprintln!("DEBUG: Created Empty Queue with size {n}");
        
        Box::new(lfq)
        
        /*
        unsafe {
            // 1) Allocate LFRing on the heap
            let lfq = std::alloc::alloc(Layout::new::<LFRing>()) as *mut LFRing;
            let lfq = match NonNull::new(lfq) {
                Some(lfq) => lfq,
                None => std::alloc::handle_alloc_error(Layout::new::<LFRing>()),
            };

            // 2) Initialize header (head, tail, threshold all zero)
            LFRing::addr_of_header(lfq).as_ptr().write(LFRingHeader {
                head: AtomicU64::new(0),
                tail: AtomicU64::new(0),
                threshold: AtomicI64::new(-1),
            });

            // 3) Initialize array with all -1 (empty entries)
            let size = pow2(order + 1);
            let array: Box<[AtomicU64]> = (0..size)
                .map(|_| AtomicU64::new(u64::MAX))  // -1 in unsigned representation
                .collect::<Vec<_>>()
                .into_boxed_slice();

            LFRing::addr_of_queue(lfq).as_ptr().write(array);

            // 4) Convert to Box and return
            Box::from_raw(lfq.as_ptr())
        }
         */
        

    }

    /// Initializes a new LFRing with all slots full.
    /// The first `half` slots contain valid indices, the remaining are marked -1.
    /// 
    /// # Safety
    /// 
    /// Safe if the system has enough memory to allocate the ring structure and array.
    pub(crate) fn init_full(order: usize) -> Box<LFRing> {

        let half = pow2(order);
        let n = half << 1;  

        // let mut array_vec = Vec::with_capacity(n);

        // // First half: map indices [0..half) to array positions
        // for i in 0..half {
        //     // let idx = n + raw_map(i as u64, order, half);
        //     let idx = n + raw_map(i as u64, order + 1, n);
        //     array_vec.push(AtomicU64::new(idx as u64));
        // }
        
        // // Second half: mark as empty with -1
        // for _ in half..n {
        //     array_vec.push(AtomicU64::new(u64::MAX));
        // }

        // let array: Box<[AtomicU64]> = array_vec.into_boxed_slice();

        let array = (0..n)
            .map(|_| AtomicU64::new(u64::MAX))
            .collect::<Vec<_>>();
        let mut array = array.into_boxed_slice();

        for i in 0..half {
            let mapped = map(i as u64, order, n);
            let value = (n + raw_map(i as u64, order, half)) as u64;
            array[mapped].store(value, Relaxed);                        // can Relaxed ordering work here?
        }

        let lfq = LFRing {
            head: AtomicU64::new(0),
            tail: AtomicU64::new(half as u64),
            threshold: AtomicI64::new(threshold_value(half, n)),
            array,
        };

        eprintln!("DEBUG: Created Full Queue with size {n}");

        Box::new(lfq)

    }

    /// Enqueues an index into the ring buffer.
    ///
    /// Returns `true` if the enqueue succeeded, `false` if the queue is full.
    ///
    /// # Safety
    ///
    /// Safe; uses proper atomic ordering and loop-based retry.
    /// 
    /// # Debugging
    /// 
    /// Does not indicate when queue is full or return false if so. Incorporate this.
    /// 
    pub(crate) fn enqueue(&self, order: usize, eidx: usize, nonempty: bool) -> bool {
        let half = pow2(order);                                                 // usize
        let n = half << 1;                                                      // usize
        let eidx = eidx ^ (n - 1);  // XOR with mask                            // usize

        loop {
            let tail = self.tail.fetch_add(1, AcqRel);                 // lfatomic_t
            let tcycle = (tail << 1) | (2 * n as u64 - 1);                        // lfatomic_t
            let tidx = map(tail, order, n);                                 // usize
            let mut entry = self.array[tidx].load(Acquire);                 // lfatomic_t

            loop {
                let ecycle = (entry << 1) | (2 * n as u64 - 1);                    // lfatomic_t

                // Check if we can write to this slot:
                // 1. ecycle < tcycle (slot is behind our cycle)
                // 2. entry == ecycle (slot is exactly at expected cycle)
                //    OR (entry == (ecycle ^ n) AND head <= tail) (slot is at last index of lfq and the head is before tail)
                let can_write = lfring_cmp_lt(ecycle, tcycle) &&                // ecycle and tcycle are stored as signed for the cmp statements, change?
                    ((entry == ecycle) || 
                     (entry == (ecycle ^ n as u64) && 
                      lfring_cmp_lt(self.head.load(Acquire), tail)));

                if can_write {
                    let new_value = (tcycle as u64) ^ (eidx as u64);
                    match self.array[tidx].compare_exchange_weak(entry, new_value, AcqRel, Acquire) {
                        Ok(_) => {
                            // Successfully wrote; update threshold if needed
                            if !nonempty && self.threshold.load(Acquire) != threshold_value(half, n) {      // Should threshold be less than? Why do we use != instead of < ?
                                self.threshold.store(threshold_value(half, n), Release);
                            }
                            return true;
                        }
                        Err(actual) => {
                            // Retry with new value
                            entry = actual;
                            continue;
                        }
                    }
                }
                break;      // Go back to retry if can't write
            }
        }
    }

    /// Dequeues an index from the ring buffer.
    ///
    /// Returns the dequeued index if successful, or `LFRING_EMPTY` if the queue is empty.
    ///
    /// # Safety
    ///
    /// Safe; uses proper atomic ordering and loop-based retry.
    pub(crate) fn dequeue(&self, order: usize, nonempty: bool) -> usize {
        let n = pow2(order + 1);

        // Exit if queue is empty, either through nonempty parameter or threshold = -1
        if !nonempty && self.threshold.load(Acquire) < 0 {
            eprintln!("lfring.dequeue(): Queue is empty");
            return LFRING_EMPTY;
        }

        loop {
            let head = self.head.fetch_add(1, AcqRel);
            let hcycle = (head << 1) | (2 * n as u64 - 1);         
            let hidx = map(head, order, n);
            let mut attempt = 0u32;

            'again: loop {
                let mut entry = self.array[hidx].load(Acquire);

                'doo: loop {
                    let ecycle = (entry << 1) | (2 * n as u64 - 1);

                    // Found a ready slot
                    if ecycle as u64 == hcycle as u64 {
                        self.array[hidx].fetch_or(n as u64 - 1, AcqRel);
                        return (entry & (n as u64 - 1)) as usize;
                    }

                    // Determine the new value to try
                    let entry_new = if (entry | n as u64) != hcycle {
                        // Slot is not in the expected cycle; clear the low bit
                        entry & !(n as u64)
                    } else {
                        // Slot cycle matches expected; try advancing the cycle
                        if attempt < 10000 {
                            attempt += 1;
                            continue 'again;
                        }
                        hcycle as u64 ^ ((!entry) & (n as u64))
                    };

                    if entry == entry_new {
                        break;  // No change needed
                    }

                    // Try to update the slot
                    if lfring_cmp_lt(ecycle, hcycle) {
                        match self.array[hidx].compare_exchange_weak(entry, entry_new, AcqRel, Acquire) {
                            Ok(_) => continue 'doo,
                            Err(actual) => {
                                entry = actual;
                                continue 'doo;
                            }
                        }
                    }
                    break 'doo;
                }
                break 'again;
            }

            // Check if queue is empty
            if !nonempty {
                let tail = self.tail.load(Acquire);
                if lfring_cmp_le(tail, head + 1) {
                    // Queue is at or before our position; try to catch up
                    self.__lfring_catchup(tail, head + 1);
                    self.threshold.fetch_sub(1, AcqRel);
                    return LFRING_EMPTY;
                }
            }

            // Decrement threshold; if it goes to 0, queue is empty
            let prev_threshold = self.threshold.fetch_sub(1, AcqRel);
            if prev_threshold <= 0 {
                return LFRING_EMPTY;
            }
        }
    }

    /// Helper: Catch up tail to head.
    /// Retries CAS until tail >= head or CAS succeeds.
    fn __lfring_catchup(&self, mut tail: u64, head: u64) {
        loop {
            match self.tail.compare_exchange_weak(tail, head, AcqRel, Acquire) {
                Ok(_) => break,
                Err(actual_tail) => {
                    tail = actual_tail;
                    let actual_head = self.head.load(Acquire);
                    if lfring_cmp_le(tail, actual_head) {
                        // tail >= head, no need to continue
                        break;
                    }
                }
            }
        }
    }
}