use std::sync::Arc;

use anyhow::Result;
use bytes::{BufMut, BytesMut};
use tokio::runtime::Builder;

use crucible::*;

fn main() -> Result<()> {
    let opt = opts()?;

    let runtime = Builder::new_multi_thread()
        .worker_threads(10)
        .thread_name("crucible-tokio")
        .enable_all()
        .build()
        .unwrap();

    /*
     * The structure we use to send work from outside crucible into the
     * Upstairs main task.
     * We create this here instead of inside up_main() so we can use
     * the run_scope() function to submit test work.
     */
    let guest = Arc::new(Guest::new());

    runtime.spawn(up_main(opt, guest.clone()));
    println!("runtime is spawned");

    /*
     * The rest of this is just test code
     */
    std::thread::sleep(std::time::Duration::from_secs(1));
    run_single_workload(&guest)?;
    println!("Tests done, wait");
    std::thread::sleep(std::time::Duration::from_secs(5));
    println!("all Tests done");
    Ok(())
}

/*
 * This is a test workload that generates a write spanning an extent
 * then trys to read the same.
 */
fn run_single_workload(guest: &Arc<Guest>) -> Result<()> {
    let my_offset = 512 * 99;
    let mut data = BytesMut::with_capacity(512 * 2);
    for seed in 4..6 {
        data.put(&[seed; 512][..]);
    }
    let data = data.freeze();
    let wio = BlockOp::Write {
        offset: my_offset,
        data,
    };
    println!("send a write");
    guest.send(wio);

    println!("send a flush");
    guest.send(BlockOp::Flush);

    let read_offset = my_offset;
    const READ_SIZE: usize = 1024;
    let mut data = BytesMut::with_capacity(READ_SIZE);
    data.put(&[0x99; READ_SIZE][..]);
    println!("send read, data at {:p}", data.as_ptr());
    let rio = BlockOp::Read {
        offset: read_offset,
        data,
    };
    guest.send(rio);

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    /*
     * Beware, if you change these defaults, then you will have to change
     * all the hard coded tests below that use make_upstairs().
     */
    fn make_upstairs() -> Arc<Upstairs> {
        let def = RegionDefinition {
            block_size: 512,
            extent_size: 100,
            extent_count: 10,
        };

        Arc::new(Upstairs {
            work: Mutex::new(Work {
                active: HashMap::new(),
                completed: AllocRingBuffer::with_capacity(2),
                next_id: 1000,
            }),
            versions: Mutex::new(Vec::new()),
            dirty: Mutex::new(Vec::new()),
            ddef: Mutex::new(def),
            downstairs: Mutex::new(Vec::with_capacity(1)),
            guest: Arc::new(Guest::new()),
        })
    }

    #[test]
    fn off_to_extent_basic() {
        /*
         * Verify the offsets match the expected block_offset for the
         * default size region.
         */
        let up = make_upstairs();

        let exv = vec![(0, 0, 512)];
        assert_eq!(extent_from_offset(&up, 0, 512).unwrap(), exv);
        let exv = vec![(0, 1, 512)];
        assert_eq!(extent_from_offset(&up, 512, 512).unwrap(), exv);
        let exv = vec![(0, 2, 512)];
        assert_eq!(extent_from_offset(&up, 1024, 512).unwrap(), exv);
        let exv = vec![(0, 3, 512)];
        assert_eq!(extent_from_offset(&up, 1024 + 512, 512).unwrap(), exv);
        let exv = vec![(0, 99, 512)];
        assert_eq!(extent_from_offset(&up, 51200 - 512, 512).unwrap(), exv);

        let exv = vec![(1, 0, 512)];
        assert_eq!(extent_from_offset(&up, 51200, 512).unwrap(), exv);
        let exv = vec![(1, 1, 512)];
        assert_eq!(extent_from_offset(&up, 51200 + 512, 512).unwrap(), exv);
        let exv = vec![(1, 2, 512)];
        assert_eq!(extent_from_offset(&up, 51200 + 1024, 512).unwrap(), exv);
        let exv = vec![(1, 99, 512)];
        assert_eq!(extent_from_offset(&up, 102400 - 512, 512).unwrap(), exv);

        let exv = vec![(2, 0, 512)];
        assert_eq!(extent_from_offset(&up, 102400, 512).unwrap(), exv);

        let exv = vec![(9, 99, 512)];
        assert_eq!(
            extent_from_offset(&up, (512 * 100 * 10) - 512, 512).unwrap(),
            exv
        );
    }

    #[test]
    fn off_to_extent_buffer() {
        /*
         * Testing a buffer size larger than the default 512
         */
        let up = make_upstairs();

        let exv = vec![(0, 0, 1024)];
        assert_eq!(extent_from_offset(&up, 0, 1024).unwrap(), exv);
        let exv = vec![(0, 1, 1024)];
        assert_eq!(extent_from_offset(&up, 512, 1024).unwrap(), exv);
        let exv = vec![(0, 2, 1024)];
        assert_eq!(extent_from_offset(&up, 1024, 1024).unwrap(), exv);
        let exv = vec![(0, 98, 1024)];
        assert_eq!(extent_from_offset(&up, 51200 - 1024, 1024).unwrap(), exv);

        let exv = vec![(1, 0, 1024)];
        assert_eq!(extent_from_offset(&up, 51200, 1024).unwrap(), exv);
        let exv = vec![(1, 1, 1024)];
        assert_eq!(extent_from_offset(&up, 51200 + 512, 1024).unwrap(), exv);
        let exv = vec![(1, 2, 1024)];
        assert_eq!(extent_from_offset(&up, 51200 + 1024, 1024).unwrap(), exv);
        let exv = vec![(1, 98, 1024)];
        assert_eq!(extent_from_offset(&up, 102400 - 1024, 1024).unwrap(), exv);

        let exv = vec![(2, 0, 1024)];
        assert_eq!(extent_from_offset(&up, 102400, 1024).unwrap(), exv);

        let exv = vec![(9, 98, 1024)];
        assert_eq!(
            extent_from_offset(&up, (512 * 100 * 10) - 1024, 1024).unwrap(),
            exv
        );
    }

    #[test]
    fn off_to_extent_vbuff() {
        let up = make_upstairs();

        /*
         * Walk the buffer sizes from 512 to the whole extent, make sure
         * it all works as expected
         */
        for bsize in (512..=51200).step_by(512) {
            let exv = vec![(0, 0, bsize)];
            assert_eq!(extent_from_offset(&up, 0, bsize).unwrap(), exv);
        }
    }

    #[test]
    fn off_to_extent_bridge() {
        /*
         * Testing when our buffer crosses extents.
         */
        let up = make_upstairs();
        /*
         * 1024 buffer
         */
        let exv = vec![(0, 99, 512), (1, 0, 512)];
        assert_eq!(extent_from_offset(&up, 51200 - 512, 1024).unwrap(), exv);
        let exv = vec![(0, 98, 1024), (1, 0, 1024)];
        assert_eq!(extent_from_offset(&up, 51200 - 1024, 2048).unwrap(), exv);

        /*
         * Largest buffer
         */
        let exv = vec![(0, 1, 51200 - 512), (1, 0, 512)];
        assert_eq!(extent_from_offset(&up, 512, 51200).unwrap(), exv);
        let exv = vec![(0, 2, 51200 - 1024), (1, 0, 1024)];
        assert_eq!(extent_from_offset(&up, 1024, 51200).unwrap(), exv);
        let exv = vec![(0, 4, 51200 - 2048), (1, 0, 2048)];
        assert_eq!(extent_from_offset(&up, 2048, 51200).unwrap(), exv);

        /*
         * Largest buffer, last block offset possible
         */
        let exv = vec![(0, 99, 512), (1, 0, 51200 - 512)];
        assert_eq!(extent_from_offset(&up, 51200 - 512, 51200).unwrap(), exv);
    }

    /*
     * Testing various invalid inputs
     */
    #[test]
    #[should_panic]
    fn off_to_extent_length_zero() {
        let up = make_upstairs();
        extent_from_offset(&up, 0, 0).unwrap();
    }
    #[test]
    #[should_panic]
    fn off_to_extent_block_align() {
        let up = make_upstairs();
        extent_from_offset(&up, 0, 511).unwrap();
    }
    #[test]
    #[should_panic]
    fn off_to_extent_block_align2() {
        let up = make_upstairs();
        extent_from_offset(&up, 0, 513).unwrap();
    }
    #[test]
    #[should_panic]
    fn off_to_extent_length_big() {
        let up = make_upstairs();
        extent_from_offset(&up, 0, 51200 + 512).unwrap();
    }
    #[test]
    #[should_panic]
    fn off_to_extent_offset_align() {
        let up = make_upstairs();
        extent_from_offset(&up, 511, 512).unwrap();
    }
    #[test]
    #[should_panic]
    fn off_to_extent_offset_align2() {
        let up = make_upstairs();
        extent_from_offset(&up, 513, 512).unwrap();
    }
    #[test]
    #[should_panic]
    fn off_to_extent_offset_big() {
        let up = make_upstairs();
        extent_from_offset(&up, 512000, 512).unwrap();
    }
}
