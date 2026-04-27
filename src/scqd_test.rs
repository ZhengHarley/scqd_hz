//! Testbench for SCQD-F queue with repeated push/pop operations and timing

#[cfg(test)]
mod tests {
    use crate::scqd_f;
    use std::time::Instant;

    const NUM_TIMES: usize = 10;
    const CAPACITY: usize = 1000;

    /// Test repeated push and pop operations on SCQD-F queue
    /// Measures the elapsed time between the first push and the last pop operation
    #[test]
    fn test_scqd_timing() {
        // Create a new SCQD-F channel with capacity for at least NUM_TIMES items
        let (mut tx, mut rx) = scqd_f::channel::<usize>(CAPACITY);

        // Record the start time before the first push
        let start = Instant::now();

        // Push NUM_TIMES values into the queue
        for i in 0..NUM_TIMES {
            match tx.push(i) {
                Ok(()) => {
                    println!("Pushed value: {}", i);
                }
                Err(_) => {
                    panic!("Failed to push value {} - queue is full", i);
                }
            }
        }

        println!("\n--- All {} values pushed ---\n", NUM_TIMES);

        // Pop NUM_TIMES values from the queue
        for i in 0..NUM_TIMES {
            match rx.pop() {
                Some(val) => {
                    println!("Popped value: {}", val);
                    assert_eq!(val, i, "Expected value {}, got {}", i, val);
                }
                None => {
                    panic!("Failed to pop value {}: queue is empty", i);
                }
            }
        }

        // Record the end time after the last pop
        let end = Instant::now();
        let elapsed = end.duration_since(start);

        println!("\n--- All {} values popped ---\n", NUM_TIMES);
        println!("Elapsed time: {:?}", elapsed);
        println!("Time per operation: {:?}", elapsed / (NUM_TIMES as u32 * 2));
    }
}
