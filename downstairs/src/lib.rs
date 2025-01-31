// Copyright 2021 Oxide Computer Company
#![cfg_attr(not(usdt_stable_asm), feature(asm))]
#![cfg_attr(
    all(target_os = "macos", not(usdt_stable_asm_sym)),
    feature(asm_sym)
)]

use futures::executor;
use futures::lock::{Mutex, MutexGuard};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crucible::*;
use crucible_common::{Block, CrucibleError, MAX_BLOCK_SIZE};

use anyhow::{bail, Result};
use bytes::BytesMut;
use futures::{SinkExt, StreamExt};
use rand::prelude::*;
use slog::{error, info, o, warn, Drain, Logger};
use slog_dtrace::{with_drain, ProbeRegistration};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::time::{sleep_until, Instant};
use tokio_util::codec::{FramedRead, FramedWrite};
use usdt::register_probes;
use uuid::Uuid;

pub mod admin;
mod dump;
pub mod region;
pub mod repair;
mod stats;

use region::Region;

pub use admin::run_dropshot;
pub use dump::dump_region;
pub use stats::*;

fn deadline_secs(secs: u64) -> Instant {
    Instant::now()
        .checked_add(Duration::from_secs(secs))
        .unwrap()
}

/*
 * Export the contents or partial contents of a Downstairs Region to
 * the file indicated.
 *
 * We will start from the provided start_block.
 * We will stop after "count" blocks are written to the export_path.
 */
pub fn downstairs_export<P: AsRef<Path> + std::fmt::Debug>(
    region: &mut Region,
    export_path: P,
    start_block: u64,
    mut count: u64,
) -> Result<()> {
    /*
     * Export an existing downstairs region to a file
     */
    let (block_size, extent_size, extent_count) = region.region_def();
    let space_per_extent = extent_size.byte_value();
    assert!(block_size > 0);
    assert!(space_per_extent > 0);
    assert!(extent_count > 0);
    assert!(space_per_extent > 0);
    let file_size = space_per_extent * extent_count as u64;

    if count == 0 {
        count = extent_size.value * extent_count as u64;
    }

    println!(
        "Export total_size: {}  Extent size:{}  Total Extents:{}",
        file_size, space_per_extent, extent_count
    );
    println!(
        "Exporting from start_block: {}  count:{}",
        start_block, count
    );

    let mut out_file = File::create(export_path)?;
    let mut blocks_copied = 0;

    'eid_loop: for eid in 0..extent_count {
        let extent_offset = space_per_extent * eid as u64;
        for block_offset in 0..extent_size.value {
            if (extent_offset + block_offset) >= start_block {
                blocks_copied += 1;

                let mut responses = region.region_read(
                    &[ReadRequest {
                        eid: eid as u64,
                        offset: Block::new_with_ddef(
                            block_offset,
                            &region.def(),
                        ),
                    }],
                    0,
                )?;
                let response = responses.pop().unwrap();

                out_file.write_all(&response.data).unwrap();

                if blocks_copied >= count {
                    break 'eid_loop;
                }
            }
        }
    }

    println!("Read and wrote out {} blocks", blocks_copied);

    Ok(())
}

/*
 * Import the contents of a file into a new Region.
 * The total size of the region will be rounded up to the next largest
 * extent multiple.
 */
pub fn downstairs_import<P: AsRef<Path> + std::fmt::Debug>(
    region: &mut Region,
    import_path: P,
) -> Result<()> {
    /*
     * Open the file to import and determine how many extents we will need
     * based on the length.
     */
    let mut f = File::open(&import_path)?;
    let file_size = f.metadata()?.len();
    let (_, extent_size, _) = region.region_def();
    let space_per_extent = extent_size.byte_value();

    let mut extents_needed = file_size / space_per_extent;
    if file_size % space_per_extent != 0 {
        extents_needed += 1;
    }
    println!(
        "Import file_size: {}  Extent size: {}  Needed extents: {}",
        file_size, space_per_extent, extents_needed
    );

    if extents_needed > region.def().extent_count().into() {
        /*
         * The file to import would require more extents than we have.
         * Extend the region to fit the file.
         */
        println!("Extending region to fit image");
        region.extend(extents_needed as u32)?;
    } else {
        println!("Region already large enough for image");
    }

    println!("Importing {:?} to region", import_path);
    let rm = region.def();

    /*
     * We want to read and write large chunks of data, rather than individual
     * blocks, to improve import performance.  The chunk buffer must be a
     * whole number of the largest block size we are able to support.
     */
    const CHUNK_SIZE: usize = 32 * 1024 * 1024;
    assert_eq!(CHUNK_SIZE % MAX_BLOCK_SIZE, 0);

    let mut offset = Block::new_with_ddef(0, &region.def());
    loop {
        let mut buffer = vec![0; CHUNK_SIZE];

        /*
         * Read data into the buffer until it is full, or we hit EOF.
         */
        let mut total = 0;
        loop {
            assert!(total <= CHUNK_SIZE);
            if total == CHUNK_SIZE {
                break;
            }

            /*
             * Rust's read guarantees that if it returns Ok(n) then
             * `0 <= n <= buffer.len()`. We have to repeatedly read until our
             * buffer is full.
             */
            let n = f.read(&mut buffer[total..])?;

            if n == 0 {
                /*
                 * We have hit EOF.  Extend the read buffer with zeroes until
                 * it is a multiple of the block size.
                 */
                while !Block::is_valid_byte_size(total, &rm) {
                    buffer[total] = 0;
                    total += 1;
                }
                break;
            }

            total += n;
        }

        if total == 0 {
            /*
             * If we read zero bytes without error, then we are done.
             */
            break;
        }

        /*
         * Use the same function upstairs uses to decide where to put the
         * data based on the LBA offset.
         */
        let nblocks = Block::from_bytes(total, &rm);
        let mut pos = Block::from_bytes(0, &rm);
        let mut writes = vec![];
        for (eid, offset) in extent_from_offset(rm, offset, nblocks).tuples() {
            let len = Block::new_with_ddef(1, &region.def());
            let data = &buffer[pos.bytes()..(pos.bytes() + len.bytes())];
            let mut buffer = BytesMut::with_capacity(data.len());
            buffer.resize(data.len(), 0);
            buffer.copy_from_slice(data);

            writes.push(crucible_protocol::Write {
                eid,
                offset,
                data: buffer.freeze(),
                block_context: BlockContext {
                    hash: integrity_hash(&[data]),
                    encryption_context: None,
                },
            });

            pos.advance(len);
        }

        // We have no job ID, so it makes no sense for accounting.
        region.region_write(&writes, 0, false)?;

        assert_eq!(nblocks, pos);
        assert_eq!(total, pos.bytes());
        offset.advance(nblocks);
    }

    /*
     * As there is no EOF indication in the downstairs, print the
     * number of total blocks we wrote to so the caller can, if they
     * want, use that to extract just this imported file.
     */
    println!(
        "Populated {} extents by copying {} bytes ({} blocks)",
        extents_needed,
        offset.byte_value(),
        offset.value,
    );

    Ok(())
}

/*
 * Debug function to dump the work list.
 */
pub async fn show_work(ds: &mut Downstairs) {
    let active_upstairs_connections = ds.active_upstairs();
    println!(
        "Active Upstairs connections: {:?}",
        active_upstairs_connections
    );

    for upstairs_connection in active_upstairs_connections {
        let work = ds.work_lock(upstairs_connection).await.unwrap();

        let mut kvec: Vec<u64> =
            work.active.keys().cloned().collect::<Vec<u64>>();

        if kvec.is_empty() {
            println!("Crucible Downstairs work queue:  Empty");
        } else {
            println!("Crucible Downstairs work queue:");
            kvec.sort_unstable();
            for id in kvec.iter() {
                let dsw = work.active.get(id).unwrap();
                let dsw_type;
                let dep_list;
                match &dsw.work {
                    IOop::Read {
                        dependencies,
                        requests: _,
                    } => {
                        dsw_type = "Read".to_string();
                        dep_list = dependencies.to_vec();
                    }
                    IOop::Write {
                        dependencies,
                        writes: _,
                    } => {
                        dsw_type = "Write".to_string();
                        dep_list = dependencies.to_vec();
                    }
                    IOop::Flush {
                        dependencies,
                        flush_number: _flush_number,
                        gen_number: _gen_number,
                        snapshot_details: _,
                    } => {
                        dsw_type = "Flush".to_string();
                        dep_list = dependencies.to_vec();
                    }
                    IOop::WriteUnwritten {
                        dependencies,
                        writes: _,
                    } => {
                        dsw_type = "WriteU".to_string();
                        dep_list = dependencies.to_vec();
                    }
                };
                println!(
                    "DSW:[{:04}] {:>05} {:>05} deps:{:?}",
                    id, dsw_type, dsw.state, dep_list,
                );
            }
        }

        println!("Done tasks {:?}", work.completed);
        println!("last_flush: {:?}", work.last_flush);
        println!("--------------------------------------");
    }
}

// DTrace probes for the downstairs
#[usdt::provider(provider = "crucible_downstairs")]
pub mod cdt {
    use crate::Arg;
    fn submit__read__start(_: u64) {}
    fn submit__writeunwritten__start(_: u64) {}
    fn submit__write__start(_: u64) {}
    fn submit__flush__start(_: u64) {}
    fn os__read__start(_: u64) {}
    fn os__writeunwritten__start(_: u64) {}
    fn os__write__start(_: u64) {}
    fn os__flush__start(_: u64) {}
    fn os__read__done(_: u64) {}
    fn os__writeunwritten__done(_: u64) {}
    fn os__write__done(_: u64) {}
    fn os__flush__done(_: u64) {}
    fn submit__read__done(_: u64) {}
    fn submit__writeunwritten__done(_: u64) {}
    fn submit__write__done(_: u64) {}
    fn submit__flush__done(_: u64) {}
}
/*
 * A new IO request has been received.
 * If the message is a ping or negotiation message, send the correct
 * response. If the message is an IO, then put the new IO the work hashmap.
 */
async fn proc_frame<WT>(
    upstairs_connection: UpstairsConnection,
    ad: &mut Arc<Mutex<Downstairs>>,
    m: &Message,
    fw: &mut Arc<Mutex<FramedWrite<WT, CrucibleEncoder>>>,
    job_channel_tx: &Arc<Mutex<Sender<u64>>>,
) -> Result<()>
where
    WT: tokio::io::AsyncWrite + std::marker::Unpin + std::marker::Send,
{
    let new_ds_id = match m {
        Message::Write {
            upstairs_id,
            session_id,
            job_id,
            dependencies,
            writes,
        } => {
            if upstairs_connection.upstairs_id != *upstairs_id {
                let mut fw = fw.lock().await;
                fw.send(Message::UuidMismatch {
                    expected_id: upstairs_connection.upstairs_id,
                })
                .await?;
                return Ok(());
            }
            if upstairs_connection.session_id != *session_id {
                let mut fw = fw.lock().await;
                fw.send(Message::UuidMismatch {
                    expected_id: upstairs_connection.session_id,
                })
                .await?;
                return Ok(());
            }
            cdt::submit__write__start!(|| *job_id);

            let new_write = IOop::Write {
                dependencies: dependencies.to_vec(),
                writes: writes.to_vec(),
            };

            let mut d = ad.lock().await;
            d.add_work(upstairs_connection, *job_id, new_write).await?;
            Some(*job_id)
        }
        Message::Flush {
            upstairs_id,
            session_id,
            job_id,
            dependencies,
            flush_number,
            gen_number,
            snapshot_details,
        } => {
            if upstairs_connection.upstairs_id != *upstairs_id {
                let mut fw = fw.lock().await;
                fw.send(Message::UuidMismatch {
                    expected_id: upstairs_connection.upstairs_id,
                })
                .await?;
                return Ok(());
            }
            if upstairs_connection.session_id != *session_id {
                let mut fw = fw.lock().await;
                fw.send(Message::UuidMismatch {
                    expected_id: upstairs_connection.session_id,
                })
                .await?;
                return Ok(());
            }
            cdt::submit__flush__start!(|| *job_id);

            let new_flush = IOop::Flush {
                dependencies: dependencies.to_vec(),
                flush_number: *flush_number,
                gen_number: *gen_number,
                snapshot_details: snapshot_details.clone(),
            };

            let mut d = ad.lock().await;
            d.add_work(upstairs_connection, *job_id, new_flush).await?;
            Some(*job_id)
        }
        Message::WriteUnwritten {
            upstairs_id,
            session_id,
            job_id,
            dependencies,
            writes,
        } => {
            if upstairs_connection.upstairs_id != *upstairs_id {
                let mut fw = fw.lock().await;
                fw.send(Message::UuidMismatch {
                    expected_id: upstairs_connection.upstairs_id,
                })
                .await?;
                return Ok(());
            }
            if upstairs_connection.session_id != *session_id {
                let mut fw = fw.lock().await;
                fw.send(Message::UuidMismatch {
                    expected_id: upstairs_connection.session_id,
                })
                .await?;
                return Ok(());
            }
            cdt::submit__writeunwritten__start!(|| *job_id);

            let new_write = IOop::WriteUnwritten {
                dependencies: dependencies.to_vec(),
                writes: writes.to_vec(),
            };

            let mut d = ad.lock().await;
            d.add_work(upstairs_connection, *job_id, new_write).await?;
            Some(*job_id)
        }
        Message::ReadRequest {
            upstairs_id,
            session_id,
            job_id,
            dependencies,
            requests,
        } => {
            if upstairs_connection.upstairs_id != *upstairs_id {
                let mut fw = fw.lock().await;
                fw.send(Message::UuidMismatch {
                    expected_id: upstairs_connection.upstairs_id,
                })
                .await?;
                return Ok(());
            }
            if upstairs_connection.session_id != *session_id {
                let mut fw = fw.lock().await;
                fw.send(Message::UuidMismatch {
                    expected_id: upstairs_connection.session_id,
                })
                .await?;
                return Ok(());
            }
            cdt::submit__read__start!(|| *job_id);

            let new_read = IOop::Read {
                dependencies: dependencies.to_vec(),
                requests: requests.to_vec(),
            };

            let mut d = ad.lock().await;
            d.add_work(upstairs_connection, *job_id, new_read).await?;
            Some(*job_id)
        }
        Message::ExtentFlush {
            repair_id,
            extent_id,
            client_id: _,
            flush_number,
            gen_number,
        } => {
            let msg = {
                let d = ad.lock().await;
                info!(
                    d.log,
                    "{} Flush extent {} with f:{} g:{}",
                    repair_id,
                    extent_id,
                    flush_number,
                    gen_number
                );
                match d.region.region_flush_extent(
                    *extent_id,
                    *flush_number,
                    *gen_number,
                    *repair_id,
                ) {
                    Ok(()) => Message::RepairAckId {
                        repair_id: *repair_id,
                    },
                    Err(e) => Message::ExtentError {
                        repair_id: *repair_id,
                        extent_id: *extent_id,
                        error: e,
                    },
                }
            };
            let mut fw = fw.lock().await;
            fw.send(msg).await?;
            return Ok(());
        }
        Message::ExtentClose {
            repair_id,
            extent_id,
        } => {
            let msg = {
                let mut d = ad.lock().await;
                info!(d.log, "{} Close extent {}", repair_id, extent_id);
                match d.region.extents.get_mut(*extent_id) {
                    Some(ext) => {
                        ext.close()?;
                        Message::RepairAckId {
                            repair_id: *repair_id,
                        }
                    }
                    None => Message::ExtentError {
                        repair_id: *repair_id,
                        extent_id: *extent_id,
                        error: CrucibleError::InvalidExtent,
                    },
                }
            };
            let mut fw = fw.lock().await;
            fw.send(msg).await?;
            return Ok(());
        }
        Message::ExtentRepair {
            repair_id,
            extent_id,
            source_client_id,
            source_repair_address,
            dest_clients,
        } => {
            let msg = {
                let mut d = ad.lock().await;
                info!(
                    d.log,
                    "{} Repair extent {} source:[{}] {:?} dest:{:?}",
                    repair_id,
                    extent_id,
                    source_client_id,
                    source_repair_address,
                    dest_clients
                );
                match d
                    .region
                    .repair_extent(*extent_id, *source_repair_address)
                    .await
                {
                    Ok(()) => Message::RepairAckId {
                        repair_id: *repair_id,
                    },
                    Err(e) => Message::ExtentError {
                        repair_id: *repair_id,
                        extent_id: *extent_id,
                        error: e,
                    },
                }
            };
            let mut fw = fw.lock().await;
            fw.send(msg).await?;
            return Ok(());
        }
        Message::ExtentReopen {
            repair_id,
            extent_id,
        } => {
            let msg = {
                let mut d = ad.lock().await;
                info!(d.log, "{} Reopen extent {}", repair_id, extent_id);
                match d.region.reopen_extent(*extent_id) {
                    Ok(()) => Message::RepairAckId {
                        repair_id: *repair_id,
                    },
                    Err(e) => Message::ExtentError {
                        repair_id: *repair_id,
                        extent_id: *extent_id,
                        error: e,
                    },
                }
            };
            let mut fw = fw.lock().await;
            fw.send(msg).await?;
            return Ok(());
        }
        x => bail!("unexpected frame {:?}", x),
    };

    /*
     * If we added work, tell the work task to get busy.
     */
    if let Some(new_ds_id) = new_ds_id {
        job_channel_tx.lock().await.send(new_ds_id).await?;
    }

    Ok(())
}

async fn do_work_task<T>(
    ads: &mut Arc<Mutex<Downstairs>>,
    upstairs_connection: UpstairsConnection,
    mut job_channel_rx: Receiver<u64>,
    fw: &mut Arc<Mutex<FramedWrite<T, CrucibleEncoder>>>,
) -> Result<()>
where
    T: tokio::io::AsyncWrite + std::marker::Unpin,
{
    /*
     * job_channel_rx is a notification that we should look for new work.
     */
    while job_channel_rx.recv().await.is_some() {
        // Add a little time to completion for this operation.
        if ads.lock().await.lossy && random() && random() {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        if !ads.lock().await.is_active(upstairs_connection) {
            // We are not an active downstairs, wait until we are
            continue;
        }

        /*
         * Build ourselves a list of all the jobs on the work hashmap that
         * are New or DepWait.
         */
        let mut new_work = {
            if let Ok(new_work) =
                ads.lock().await.new_work(upstairs_connection).await
            {
                new_work
            } else {
                // This means we couldn't unblock jobs for this UUID
                continue;
            }
        };

        /*
         * We don't have to do jobs in order, but the dependencies are, at
         * least for now, always going to be in order of job id.  So,
         * to best move things forward it is going to be fewer laps
         * through the list if we take the lowest job id first.
         */
        new_work.sort_unstable();

        for new_id in new_work.iter() {
            if ads.lock().await.lossy && random() && random() {
                // Skip a job that needs to be done. Sometimes
                continue;
            }

            /*
             * If this job is still new, take it and go to work. The
             * in_progress method will only return a job if all
             * dependencies are met.
             */
            let job_id = ads
                .lock()
                .await
                .in_progress(upstairs_connection, *new_id)
                .await?;
            if let Some(job_id) = job_id {
                let m = ads
                    .lock()
                    .await
                    .do_work(upstairs_connection, job_id)
                    .await?;

                if let Some(m) = m {
                    ads.lock()
                        .await
                        .complete_work_stat(upstairs_connection, &m, job_id)
                        .await?;
                    // Notify the upstairs before completing work
                    let mut fw = fw.lock().await;
                    fw.send(&m).await?;
                    drop(fw);

                    ads.lock()
                        .await
                        .complete_work(upstairs_connection, job_id, m)
                        .await?;
                }
            }
        }
    }

    // None means the channel is closed
    Ok(())
}

async fn proc_stream(
    ads: &mut Arc<Mutex<Downstairs>>,
    stream: WrappedStream,
) -> Result<()> {
    match stream {
        WrappedStream::Http(sock) => {
            let (read, write) = sock.into_split();

            let fr = FramedRead::new(read, CrucibleDecoder::new());
            let fw = Arc::new(Mutex::new(FramedWrite::new(
                write,
                CrucibleEncoder::new(),
            )));

            proc(ads, fr, fw).await
        }
        WrappedStream::Https(stream) => {
            let (read, write) = tokio::io::split(stream);

            let fr = FramedRead::new(read, CrucibleDecoder::new());
            let fw = Arc::new(Mutex::new(FramedWrite::new(
                write,
                CrucibleEncoder::new(),
            )));

            proc(ads, fr, fw).await
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UpstairsConnection {
    upstairs_id: Uuid,
    session_id: Uuid,
    gen: u64,
}

/*
 * This function handles the initial negotiation steps between the
 * upstairs and the downstairs.  Either we return error, or we call
 * the next function if everything was successful and we can start
 * taking IOs from the upstairs.
 */
async fn proc<RT, WT>(
    ads: &mut Arc<Mutex<Downstairs>>,
    mut fr: FramedRead<RT, CrucibleDecoder>,
    fw: Arc<Mutex<FramedWrite<WT, CrucibleEncoder>>>,
) -> Result<()>
where
    RT: tokio::io::AsyncRead + std::marker::Unpin + std::marker::Send,
    WT: tokio::io::AsyncWrite
        + std::marker::Unpin
        + std::marker::Send
        + 'static,
{
    // In this function, repair address should exist, and shouldn't change. Grab
    // it here.
    let repair_addr = ads.lock().await.repair_address.unwrap();

    let mut negotiated = 0;
    let mut upstairs_connection: Option<UpstairsConnection> = None;

    let (_another_upstairs_active_tx, mut another_upstairs_active_rx) =
        channel::<UpstairsConnection>(1);
    let another_upstairs_active_tx = Arc::new(_another_upstairs_active_tx);

    let log = ads.lock().await.log.new(o!("task" => "proc".to_string()));
    /*
     * See the comment in the proc() function on the upstairs side that
     * describes how this negotiation takes place.
     *
     * The final step in negotiation (as dictated by the upstairs) is
     * either LastFlush, or ExtentVersionsPlease.  Once we respond to
     * that message, we can move forward and start receiving IO from
     * the upstairs.
     */
    while negotiated < 4 {
        tokio::select! {
            /*
             * Don't wait more than 50 seconds to hear from the other side.
             * XXX Timeouts, timeouts: always wrong!  Some too short and
             * some too long.
             */
            _ = sleep_until(deadline_secs(50)) => {
                bail!("did not negotiate a protocol");
            }

            /*
             * This Upstairs' thread will receive this signal when another
             * Upstairs promotes itself to active. The only way this path is
             * reached is if this Upstairs promoted itself to active, storing
             * another_upstairs_active_tx in the Downstairs active_upstairs
             * tuple.
             *
             * The two unwraps here should be safe: this thread negotiated and
             * activated, and then another did (in order to send this thread
             * this signal).
             */
            new_upstairs_connection = another_upstairs_active_rx.recv() => {
                match new_upstairs_connection {
                    None => {
                        // There shouldn't be a path through the code where we
                        // close the channel before sending a message through it
                        // (see [`promote_to_active`]), though [`clear_active`]
                        // simply drops the active_upstairs tuple - but the only
                        // place that calls `clear_active` is below when the
                        // Upstairs disconnects.
                        //
                        // We have to bail here though - the Downstairs can't be
                        // running without the ability for another Upstairs to
                        // kick out the previous one during activation.
                        bail!("another_upstairs_active_rx closed during \
                            negotiation");
                    }

                    Some(new_upstairs_connection) => {
                        // another upstairs negotiated and went active after
                        // this one did (and before this one completed
                        // negotiation)
                        let upstairs_connection = upstairs_connection.unwrap();
                        warn!(
                            log,
                            "Another upstairs {:?} promoted to active, \
                            shutting down connection for {:?}",
                            new_upstairs_connection, upstairs_connection);

                        let mut fw = fw.lock().await;
                        fw.send(Message::YouAreNoLongerActive {
                            new_upstairs_id:
                                new_upstairs_connection.upstairs_id,
                            new_session_id:
                                new_upstairs_connection.session_id,
                            new_gen: new_upstairs_connection.gen,
                        }).await?;

                        return Ok(());
                    }
                }
            }

            new_read = fr.next() => {
                /*
                 * Negotiate protocol before we take any IO requests.
                 */
                match new_read.transpose()? {
                    None => {
                        // Upstairs disconnected
                        let mut ds = ads.lock().await;

                        if let Some(upstairs_connection) = upstairs_connection {
                            info!(
                                log,
                                "upstairs {:?} disconnected, {} jobs left",
                                upstairs_connection,
                                ds.jobs(upstairs_connection).await?,
                            );

                            if ds.is_active(upstairs_connection) {
                                info!(
                                    log,
                                    "upstairs {:?} was previously \
                                    active, clearing", upstairs_connection);
                                ds.clear_active(upstairs_connection).await?;
                            }
                        } else {
                            info!(log, "unknown upstairs disconnected");
                        }

                        return Ok(());
                    }
                    Some(Message::Ruok) => {
                        let mut fw = fw.lock().await;
                        fw.send(Message::Imok).await?;
                    }
                    Some(Message::HereIAm {
                        version,
                        upstairs_id,
                        session_id,
                        gen,
                        read_only,
                        encrypted,
                    }) => {
                        if negotiated != 0 {
                            bail!("Received connect out of order {}",
                                negotiated);
                        }

                        if version != 1 {
                            bail!("expected version 1, got {}", version);
                        }

                        // Reject an Upstairs negotiation if there is a mismatch
                        // of expectation, and terminate the connection - the
                        // Upstairs will not be able to successfully negotiate.
                        {
                            let ds = ads.lock().await;
                            if ds.read_only != read_only {
                                let mut fw = fw.lock().await;

                                fw.send(Message::ReadOnlyMismatch {
                                    expected: ds.read_only,
                                }).await?;

                                bail!("closing connection due to read-only \
                                    mismatch");
                            }

                            if ds.encrypted != encrypted {
                                let mut fw = fw.lock().await;

                                fw.send(Message::EncryptedMismatch {
                                    expected: ds.encrypted,
                                }).await?;

                                bail!("closing connection due to encryption \
                                    mismatch");
                            }
                        }

                        negotiated = 1;
                        upstairs_connection = Some(UpstairsConnection {
                            upstairs_id,
                            session_id,
                            gen,
                        });
                        info!(
                            log, "upstairs {:?} connected",
                            upstairs_connection.unwrap());

                        let mut fw = fw.lock().await;
                        fw.send(
                            Message::YesItsMe { version: 1, repair_addr }
                        ).await?;
                    }
                    Some(Message::PromoteToActive {
                        upstairs_id,
                        session_id,
                        gen,
                    }) => {
                        if negotiated != 1 {
                            bail!("Received activate out of order {}",
                                negotiated);
                        }

                        // Only allowed to promote or demote self
                        let mut upstairs_connection =
                            upstairs_connection.as_mut().unwrap();
                        let matches_self =
                            upstairs_connection.upstairs_id == upstairs_id &&
                            upstairs_connection.session_id == session_id;

                        if !matches_self {
                            let mut fw = fw.lock().await;
                            fw.send(
                                Message::UuidMismatch {
                                    expected_id:
                                        upstairs_connection.upstairs_id,
                                }
                            ).await?;
                            bail!(
                                "Upstairs connection expected \
                                upstairs_id:{} session_id:{}  received \
                                upstairs_id:{} session_id:{}",
                                upstairs_connection.upstairs_id,
                                upstairs_connection.session_id,
                                upstairs_id,
                                session_id
                            );

                        } else {
                            if upstairs_connection.gen != gen {
                                warn!(
                                    log,
                                    "warning: generation number at \
                                    negotiation was {} and {} at \
                                    activation, updating",
                                    upstairs_connection.gen,
                                    gen,
                                );

                                upstairs_connection.gen = gen;
                            }

                            {
                                let mut ds = ads.lock().await;

                                ds.promote_to_active(
                                    *upstairs_connection,
                                    another_upstairs_active_tx.clone()
                                ).await?;
                            }
                            negotiated = 2;

                            let mut fw = fw.lock().await;
                            fw.send(Message::YouAreNowActive {
                                upstairs_id,
                                session_id,
                                gen,
                            }).await?;
                        }
                    }
                    Some(Message::RegionInfoPlease) => {
                        if negotiated != 2 {
                            bail!("Received RegionInfo out of order {}",
                                negotiated);
                        }
                        negotiated = 3;
                        let region_def = {
                            let ds = ads.lock().await;
                            ds.region.def()
                        };

                        let mut fw = fw.lock().await;
                        fw.send(Message::RegionInfo { region_def }).await?;
                    }
                    Some(Message::LastFlush { last_flush_number }) => {
                        if negotiated != 3 {
                            bail!("Received LastFlush out of order {}",
                                negotiated);
                        }

                        negotiated = 4;

                        {
                            let mut ds = ads.lock().await;
                            let mut work = ds.work_lock(
                                upstairs_connection.unwrap(),
                            ).await?;
                            work.last_flush = last_flush_number;
                            info!(
                                log,
                                "Set last flush {}", last_flush_number);
                        }

                        let mut fw = fw.lock().await;
                        fw.send(Message::LastFlushAck {
                            last_flush_number
                        }).await?;

                        /*
                         * Once this command is sent, we are ready to exit
                         * the loop and move forward with receiving IOs
                         */
                    }
                    Some(Message::ExtentVersionsPlease) => {
                        if negotiated != 3 {
                            bail!("Received ExtentVersions out of order {}",
                                negotiated);
                        }
                        negotiated = 4;
                        let ds = ads.lock().await;
                        let flush_numbers = ds.region.flush_numbers()?;
                        let gen_numbers = ds.region.gen_numbers()?;
                        let dirty_bits = ds.region.dirty()?;
                        drop(ds);

                        let mut fw = fw.lock().await;
                        fw.send(Message::ExtentVersions {
                            gen_numbers,
                            flush_numbers,
                            dirty_bits,
                        })
                        .await?;

                        /*
                         * Once this command is sent, we are ready to exit
                         * the loop and move forward with receiving IOs
                         */
                    }
                    Some(_msg) => {
                        warn!(
                            log,
                            "Ignored message received during negotiation"
                        );
                    }
                }
            }
        }
    }

    info!(log, "Downstairs has completed Negotiation");
    assert!(upstairs_connection.is_some());
    let upstairs_connection = upstairs_connection.unwrap();

    resp_loop(ads, fr, fw, another_upstairs_active_rx, upstairs_connection)
        .await
}

/*
 * This function listens for and answers requests from the upstairs.
 * We assume here that correct negotiation has taken place and this
 * downstairs is ready to receive IO.
 */
async fn resp_loop<RT, WT>(
    ads: &mut Arc<Mutex<Downstairs>>,
    mut fr: FramedRead<RT, CrucibleDecoder>,
    fw: Arc<Mutex<FramedWrite<WT, CrucibleEncoder>>>,
    mut another_upstairs_active_rx: mpsc::Receiver<UpstairsConnection>,
    upstairs_connection: UpstairsConnection,
) -> Result<()>
where
    RT: tokio::io::AsyncRead + std::marker::Unpin + std::marker::Send,
    WT: tokio::io::AsyncWrite
        + std::marker::Unpin
        + std::marker::Send
        + 'static,
{
    let mut lossy_interval = deadline_secs(5);
    // Create the log for this task to use.
    let log = ads.lock().await.log.new(o!("task" => "main".to_string()));

    // XXX flow control size to double what Upstairs has for upper limit?
    let (_job_channel_tx, job_channel_rx) = channel(200);
    let job_channel_tx = Arc::new(Mutex::new(_job_channel_tx));

    /*
     * Create tasks for:
     *  Doing the work then sending the ACK
     *  Pulling work off the socket and putting on the work queue.
     *
     * These tasks and this function must be able to handle the
     * Upstairs connection going away at any time, as well as a forced
     * migration where a new Upstairs connects and the old (current from
     * this threads point of view) work is discarded.
     * As migration or upstairs failure can happen at any time, this
     * function must watch for tasks going away and handle that
     * gracefully.  By exiting the loop here, we allow the calling
     * function to take over and handle a reconnect or a new upstairs
     * takeover.
     */
    let dw_task = {
        let mut adc = ads.clone();
        let mut fwc = fw.clone();
        tokio::spawn(async move {
            do_work_task(
                &mut adc,
                upstairs_connection,
                job_channel_rx,
                &mut fwc,
            )
            .await
        })
    };

    let (message_channel_tx, mut message_channel_rx) = channel(200);
    let pf_task = {
        let mut adc = ads.clone();
        let tx = job_channel_tx.clone();
        let mut fwc = fw.clone();
        tokio::spawn(async move {
            while let Some(m) = message_channel_rx.recv().await {
                if let Err(e) =
                    proc_frame(upstairs_connection, &mut adc, &m, &mut fwc, &tx)
                        .await
                {
                    bail!("Proc frame returns error: {}", e);
                }
            }
            Ok(())
        })
    };
    let lossy = ads.lock().await.lossy;

    tokio::pin!(dw_task);
    tokio::pin!(pf_task);
    loop {
        tokio::select! {
            e = &mut dw_task => {
                bail!("do_work_task task has ended: {:?}", e);
            }
            e = &mut pf_task => {
                bail!("pf task ended: {:?}", e);
            }
            /*
             * If we have set "lossy", then we need to check every now and
             * then that there were not skipped jobs that we need to go back
             * and finish up. If lossy is not set, then this should only
             * trigger once then never again.
             */
            _ = sleep_until(lossy_interval), if lossy => {
                //let ds = ads.lock().await;
                //show_work(&ds).await;
                job_channel_tx.lock().await.send(0).await?;
                lossy_interval = deadline_secs(5);
            }
            /*
             * Don't wait more than 50 seconds to hear from the other side.
             * XXX Timeouts, timeouts: always wrong!  Some too short and
             * some too long.
             */
            _ = sleep_until(deadline_secs(50)) => {
                bail!("inactivity timeout");
            }

            /*
             * This Upstairs' thread will receive this signal when another
             * Upstairs promotes itself to active. The only way this path is
             * reached is if this Upstairs promoted itself to active, storing
             * another_upstairs_active_tx in the Downstairs active_upstairs
             * tuple.
             *
             * The two unwraps here should be safe: this thread negotiated and
             * activated, and then another did (in order to send this thread
             * this signal).
             */
            new_upstairs_connection = another_upstairs_active_rx.recv() => {
                match new_upstairs_connection {
                    None => {
                        // There shouldn't be a path through the code where we
                        // close the channel before sending a message through it
                        // (see [`promote_to_active`]), though [`clear_active`]
                        // simply drops the active_upstairs tuple - but the only
                        // place that calls `clear_active` is below when the
                        // Upstairs disconnects.
                        //
                        // We have to bail here though - the Downstairs can't be
                        // running without the ability for another Upstairs to
                        // kick out the previous one during activation.
                        bail!("another_upstairs_active_rx closed during \
                            resp_loop");
                    }

                    Some(new_upstairs_connection) => {
                        // another upstairs negotiated and went active after
                        // this one did
                        warn!(
                            log,
                            "Another upstairs {:?} promoted to active, \
                            shutting down connection for {:?}",
                            new_upstairs_connection, upstairs_connection);

                        let mut fw = fw.lock().await;
                        fw.send(Message::YouAreNoLongerActive {
                            new_upstairs_id:
                                new_upstairs_connection.upstairs_id,
                            new_session_id:
                                new_upstairs_connection.session_id,
                            new_gen: new_upstairs_connection.gen,
                        }).await?;

                        return Ok(());
                    }
                }
            }
            new_read = fr.next() => {
                match new_read {
                    None => {
                        // Upstairs disconnected
                        let mut ds = ads.lock().await;

                        warn!(
                            log,
                            "upstairs {:?} disconnected, {} jobs left",
                            upstairs_connection, ds.jobs(upstairs_connection).await?,
                        );

                        if ds.is_active(upstairs_connection) {
                            warn!(log, "upstairs {:?} was previously \
                                active, clearing", upstairs_connection);
                            ds.clear_active(upstairs_connection).await?;
                        }

                        return Ok(());
                    }
                    Some(Ok(msg)) => {
                        if matches!(msg, Message::Ruok) {
                            // Respond instantly to pings, don't wait.
                            let mut fw = fw.lock().await;
                            fw.send(Message::Imok).await?;
                        } else {
                            message_channel_tx.send(msg).await?;
                        }
                    }
                    Some(Err(e)) => {
                        // XXX "unexpected end of file" can occur if upstairs
                        // terminates, we don't yet have a HangUp message
                        return Err(e);
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct ActiveUpstairs {
    pub upstairs_connection: UpstairsConnection,
    pub work: Mutex<Work>,
    pub terminate_sender: Arc<Sender<UpstairsConnection>>,
}

/*
 * Overall structure for things the downstairs is tracking.
 * This includes the extents and their status as well as the
 * downstairs work queue.
 */
#[derive(Debug)]
pub struct Downstairs {
    pub region: Region,
    lossy: bool,         // Test flag, enables pauses and skipped jobs
    return_errors: bool, // Test flag
    active_upstairs: HashMap<Uuid, ActiveUpstairs>,
    dss: DsStatOuter,
    read_only: bool,
    encrypted: bool,
    pub address: Option<SocketAddr>,
    pub repair_address: Option<SocketAddr>,
    log: Logger,
}

impl Downstairs {
    fn new(
        region: Region,
        lossy: bool,
        return_errors: bool,
        read_only: bool,
        encrypted: bool,
        log: Logger,
    ) -> Self {
        let dss = DsStatOuter {
            ds_stat_wrap: Arc::new(Mutex::new(DsCountStat::new(
                region.def().uuid(),
            ))),
        };
        Downstairs {
            region,
            lossy,
            return_errors,
            active_upstairs: HashMap::new(),
            dss,
            read_only,
            encrypted,
            address: None,
            repair_address: None,
            log,
        }
    }

    /*
     * Only grab the lock if the UpstairsConnection matches.
     *
     * Multiple Upstairs connecting to this Downstairs will spawn multiple
     * threads that all can potentially add work to the same `active` hash
     * map. Only one Upstairs can be "active" at any one time though.
     * When promote_to_active takes the work lock, it will clear out the
     * `active` hash map and (if applicable) will signal to the currently
     * active Upstairs to terminate the connection.
     *
     * `new_work` and `add_work` both grab their work lock through this
     * function. Let's say `promote_to_active` and `add_work` are racing for
     * the work lock. If `add_work` wins the race it will put work into
     * `active`, then `promote_to_active` will clear it out. If
     * `promote_to_active` wins the race, it will change the Downstairs'
     * active UpstairsConnection, and send the terminate signal to the
     * tasks that are communicating to the previously active Upstairs
     * (along with terminating the Downstairs tasks). If `add_work` for
     * the previous Upstairs then does fire, it will fail to
     * grab the lock because the UpstairsConnection is no longer active, and
     * that `add_work` thread should close.
     *
     * Let's say `new_work` and `promote_to_active` are racing. If `new_work`
     * wins, then it will return and run those jobs in `do_work_task`.
     * However, `promote_to_active` will grab the lock and change the
     * active UpstairsConnection, causing `do_work` to return
     * UpstairsInactive for the jobs that were just returned. If
     * `promote_to_active` wins, it will clear out the jobs of the old
     * Upstairs.
     *
     * Grabbing the lock in this way should properly clear out the previously
     * active Upstairs without causing jobs to be incorrectly sent to the
     * newly active Upstairs.
     */
    async fn work_lock(
        &mut self,
        upstairs_connection: UpstairsConnection,
    ) -> Result<MutexGuard<'_, Work>> {
        let upstairs_uuid = upstairs_connection.upstairs_id;
        if !self.active_upstairs.contains_key(&upstairs_uuid) {
            warn!(
                self.log,
                "{:?} cannot grab work lock, {} is not active!",
                upstairs_connection,
                upstairs_uuid,
            );

            bail!(CrucibleError::UpstairsInactive);
        }

        let active_upstairs = self.active_upstairs.get(&upstairs_uuid).unwrap();

        if active_upstairs.upstairs_connection != upstairs_connection {
            warn!(
                self.log,
                "{:?} cannot grab lock, does not match {:?}!",
                upstairs_connection,
                active_upstairs.upstairs_connection,
            );

            bail!(CrucibleError::UpstairsInactive)
        }

        Ok(active_upstairs.work.lock().await)
    }

    async fn jobs(
        &mut self,
        upstairs_connection: UpstairsConnection,
    ) -> Result<usize> {
        let work = self.work_lock(upstairs_connection).await?;
        Ok(work.jobs())
    }

    async fn new_work(
        &mut self,
        upstairs_connection: UpstairsConnection,
    ) -> Result<Vec<u64>> {
        let work = self.work_lock(upstairs_connection).await?;
        Ok(work.new_work(upstairs_connection))
    }

    // Add work to the Downstairs
    async fn add_work(
        &mut self,
        upstairs_connection: UpstairsConnection,
        ds_id: u64,
        work: IOop,
    ) -> Result<()> {
        // The Upstairs will send Flushes periodically, even in read only mode
        // we have to accept them. But read-only should never accept writes!
        if self.read_only {
            let is_write = match work {
                IOop::Write { .. } | IOop::WriteUnwritten { .. } => true,
                IOop::Read { .. } | IOop::Flush { .. } => false,
            };

            if is_write {
                error!(self.log, "read-only but received write {:?}", work);
                bail!(CrucibleError::ModifyingReadOnlyRegion);
            }
        }

        let dsw = DownstairsWork {
            upstairs_connection,
            ds_id,
            work,
            state: WorkState::New,
        };

        let mut work = self.work_lock(upstairs_connection).await?;
        work.add_work(ds_id, dsw);

        Ok(())
    }

    #[cfg(test)]
    async fn get_job(
        &mut self,
        upstairs_connection: UpstairsConnection,
        ds_id: u64,
    ) -> Result<DownstairsWork> {
        let mut work = self.work_lock(upstairs_connection).await?;
        Ok(work.get_job(ds_id))
    }

    // Downstairs, move a job to in_progress, if we can
    async fn in_progress(
        &mut self,
        upstairs_connection: UpstairsConnection,
        ds_id: u64,
    ) -> Result<Option<u64>> {
        let job = {
            let log = self.log.new(o!("role" => "work".to_string()));
            let mut work = self.work_lock(upstairs_connection).await?;
            work.in_progress(ds_id, log)
        };

        if let Some((job_id, upstairs_connection)) = job {
            if !self.is_active(upstairs_connection) {
                // Don't return a job with the wrong uuid! `promote_to_active`
                // should have removed any active jobs, and
                // `work.new_work` should have filtered on the correct UUID.
                panic!("Don't return a job for a non-active connection!");
            }

            Ok(Some(job_id))
        } else {
            Ok(None)
        }
    }

    // Given a job ID, do the work for that IO.
    //
    // This method calls into the Downstair's region and performs the read /
    // write / flush action.
    async fn do_work(
        &mut self,
        upstairs_connection: UpstairsConnection,
        job_id: u64,
    ) -> Result<Option<Message>> {
        let job = {
            let mut work = self.work_lock(upstairs_connection).await?;
            let job = work.get_ready_job(job_id).await;

            // `promote_to_active` can clear out the Work struct for this
            // UpstairsConnection, but the tasks can still be working on
            // outdated job IDs. If that happens, `get_ready_job` will return a
            // None, so bail early here.
            if job.is_none() {
                return Ok(None);
            }

            job.unwrap()
        };

        match &job.work {
            IOop::Read {
                dependencies: _dependencies,
                requests,
            } => {
                /*
                 * Any error from an IO should be intercepted here and passed
                 * back to the upstairs.
                 */
                let responses = if self.return_errors && random() && random() {
                    warn!(self.log, "returning error on read!");
                    Err(CrucibleError::GenericError("test error".to_string()))
                } else if !self.is_active(job.upstairs_connection) {
                    error!(self.log, "Upstairs inactive error");
                    Err(CrucibleError::UpstairsInactive)
                } else {
                    self.region.region_read(requests, job_id)
                };

                Ok(Some(Message::ReadResponse {
                    upstairs_id: job.upstairs_connection.upstairs_id,
                    session_id: job.upstairs_connection.session_id,
                    job_id: job.ds_id,
                    responses,
                }))
            }
            IOop::WriteUnwritten {
                dependencies: _dependencies,
                writes,
            } => {
                /*
                 * Any error from an IO should be intercepted here and passed
                 * back to the upstairs.
                 */
                let result = if self.return_errors && random() && random() {
                    warn!(self.log, "returning error on writeunwritten!");
                    Err(CrucibleError::GenericError("test error".to_string()))
                } else if !self.is_active(job.upstairs_connection) {
                    error!(self.log, "Upstairs inactive error");
                    Err(CrucibleError::UpstairsInactive)
                } else {
                    // The region_write will handle what happens to each block
                    // based on if they have data or not.
                    self.region.region_write(writes, job_id, true)
                };

                Ok(Some(Message::WriteUnwrittenAck {
                    upstairs_id: job.upstairs_connection.upstairs_id,
                    session_id: job.upstairs_connection.session_id,
                    job_id: job.ds_id,
                    result,
                }))
            }
            IOop::Write {
                dependencies: _dependencies,
                writes,
            } => {
                let result = if self.return_errors && random() && random() {
                    warn!(self.log, "returning error on write!");
                    Err(CrucibleError::GenericError("test error".to_string()))
                } else if !self.is_active(job.upstairs_connection) {
                    error!(self.log, "Upstairs inactive error");
                    Err(CrucibleError::UpstairsInactive)
                } else {
                    self.region.region_write(writes, job_id, false)
                };

                Ok(Some(Message::WriteAck {
                    upstairs_id: job.upstairs_connection.upstairs_id,
                    session_id: job.upstairs_connection.session_id,
                    job_id: job.ds_id,
                    result,
                }))
            }
            IOop::Flush {
                dependencies: _dependencies,
                flush_number,
                gen_number,
                snapshot_details,
            } => {
                let result = if self.return_errors && random() && random() {
                    warn!(self.log, "returning error on flush!");
                    Err(CrucibleError::GenericError("test error".to_string()))
                } else if !self.is_active(job.upstairs_connection) {
                    error!(self.log, "Upstairs inactive error");
                    Err(CrucibleError::UpstairsInactive)
                } else {
                    self.region.region_flush(
                        *flush_number,
                        *gen_number,
                        snapshot_details,
                        job_id,
                    )
                };

                Ok(Some(Message::FlushAck {
                    upstairs_id: job.upstairs_connection.upstairs_id,
                    session_id: job.upstairs_connection.session_id,
                    job_id: job.ds_id,
                    result,
                }))
            }
        }
    }

    /*
     * Complete work by:
     *
     * - notifying the upstairs with the response
     * - removing the job from active
     * - removing the response
     * - putting the id on the completed list.
     */
    async fn complete_work(
        &mut self,
        upstairs_connection: UpstairsConnection,
        ds_id: u64,
        m: Message,
    ) -> Result<()> {
        let mut work = self.work_lock(upstairs_connection).await?;

        // Complete the job
        let is_flush = matches!(m, Message::FlushAck { .. });

        // If upstairs_connection grabs the work lock, it is the active
        // connection for this Upstairs UUID. The job should exist in the Work
        // struct. If it does not, then we're in the case where the same
        // Upstairs has reconnected and been promoted to active, meaning
        // `work.clear()` was run. If that's the case, then do not alter the
        // Work struct, because there's now two tasks running for the same
        // UpstairsConnection, and we're the one that should be on the way out
        // due to a message on the terminate_sender channel.
        if work.active.remove(&ds_id).is_some() {
            if is_flush {
                work.last_flush = ds_id;
                work.completed = Vec::with_capacity(32);
            } else {
                work.completed.push(ds_id);
            }
        }

        Ok(())
    }

    /*
     * After we complete a read/write/flush on a region, update the
     * Oximeter counter for the operation.
     */
    async fn complete_work_stat(
        &mut self,
        _upstairs_connection: UpstairsConnection,
        m: &Message,
        ds_id: u64,
    ) -> Result<()> {
        // XXX dss per upstairs connection?
        match m {
            Message::FlushAck { .. } => {
                cdt::submit__flush__done!(|| ds_id);
                self.dss.add_flush().await;
            }
            Message::WriteAck { .. } => {
                cdt::submit__write__done!(|| ds_id);
                self.dss.add_write().await;
            }
            Message::WriteUnwrittenAck { .. } => {
                cdt::submit__writeunwritten__done!(|| ds_id);
                self.dss.add_write().await;
            }
            Message::ReadResponse { .. } => {
                cdt::submit__read__done!(|| ds_id);
                self.dss.add_read().await;
            }
            _ => (),
        }

        Ok(())
    }

    async fn promote_to_active(
        &mut self,
        upstairs_connection: UpstairsConnection,
        tx: Arc<Sender<UpstairsConnection>>,
    ) -> Result<()> {
        if self.read_only {
            // Multiple active read-only sessions are allowed, but multiple
            // sessions for the same Upstairs UUID are not. Kick out a
            // previously active session for this UUID if one exists. Do this
            // while holding the work lock so the previously active Upstairs
            // isn't adding more work.
            if let Some(active_upstairs) =
                self.active_upstairs.get(&upstairs_connection.upstairs_id)
            {
                let mut work = active_upstairs.work.lock().await;

                info!(
                    self.log,
                    "Signaling to {:?} thread that {:?} is being \
                    promoted (read-only)",
                    active_upstairs.upstairs_connection,
                    upstairs_connection,
                );

                match futures::executor::block_on(
                    active_upstairs.terminate_sender.send(upstairs_connection),
                ) {
                    Ok(_) => {}
                    Err(e) => {
                        /*
                         * It's possible the old thread died due to some
                         * connection error. In that case the
                         * receiver will have closed and
                         * the above send will fail.
                         */
                        error!(
                            self.log,
                            "Error while signaling to {:?} thread: {:?}",
                            active_upstairs.upstairs_connection,
                            e,
                        );
                    }
                }

                // Note: in the future, differentiate between new upstairs
                // connecting vs same upstairs reconnecting here.
                //
                // Clear out active jobs, the last flush, and completed
                // information, as that will not be valid any longer.
                //
                // TODO: Really work through this error case
                if work.active.keys().len() > 0 {
                    warn!(
                        self.log,
                        "Crucible Downstairs promoting {:?} to active, \
                        discarding {} jobs",
                        upstairs_connection,
                        work.active.keys().len()
                    );
                }

                // In the future, we may decide there is some way to continue
                // working on outstanding jobs, or a way to merge. But for now,
                // we just throw out what we have and let the upstairs resend
                // anything to us that it did not get an ACK for.
                work.clear();
            } else {
                // There is no current session for this Upstairs UUID.
            }

            // Insert a new session, overwritting the previous entry if the
            // Upstairs UUID has an entry already.
            self.active_upstairs.insert(
                upstairs_connection.upstairs_id,
                ActiveUpstairs {
                    upstairs_connection,
                    work: Mutex::new(Work::new()),
                    terminate_sender: tx,
                },
            );

            Ok(())
        } else {
            // Only one active read-write session is allowed. Kick out the
            // currently active Upstairs session if one exists.
            let currently_active_upstairs_uuids: Vec<Uuid> =
                self.active_upstairs.keys().copied().collect();

            match currently_active_upstairs_uuids.len() {
                0 => {
                    // No currently active Upstairs sessions
                    self.active_upstairs.insert(
                        upstairs_connection.upstairs_id,
                        ActiveUpstairs {
                            upstairs_connection,
                            work: Mutex::new(Work::new()),
                            terminate_sender: tx,
                        },
                    );

                    assert_eq!(self.active_upstairs.len(), 1);

                    // Re-open any closed extents
                    self.region.reopen_all_extents()?;

                    info!(
                        self.log,
                        "{:?} is now active (read-write)", upstairs_connection,
                    );

                    Ok(())
                }

                1 => {
                    // There is an existing session.  Determine if this new
                    // request to promote to active should move forward or
                    // be blocked.
                    let active_upstairs = self
                        .active_upstairs
                        .get(&currently_active_upstairs_uuids[0])
                        .unwrap();

                    println!(
                        "Attempting RW takeover from {:?} to {:?}",
                        active_upstairs.upstairs_connection,
                        upstairs_connection,
                    );

                    // Compare the new generaion number to what the existing
                    // connection is and take action based on that.
                    match upstairs_connection
                        .gen
                        .cmp(&active_upstairs.upstairs_connection.gen)
                    {
                        Ordering::Less => {
                            // If the new connection has a lower generation
                            // number than the current connection, we don't
                            // allow it to take over.
                            bail!(
                                "Current gen {} is > requested gen of {}",
                                active_upstairs.upstairs_connection.gen,
                                upstairs_connection.gen,
                            );
                        }
                        Ordering::Equal => {
                            // The generation numbers match, the only way we
                            // allow this new connection to take over is if the
                            // upstairs_id and the session_id are the same,
                            // which means the whole structures need to be
                            // identical.
                            if active_upstairs.upstairs_connection
                                != upstairs_connection
                            {
                                bail!(
                                    "Same gen, but UUIDs {:?} don't match {:?}",
                                    active_upstairs.upstairs_connection,
                                    upstairs_connection,
                                );
                            }
                        }
                        // The only remaining case is the new generation
                        // number is higher than the existing.
                        Ordering::Greater => {}
                    }

                    // Now that we know we can remove/replace it, go ahead
                    // and take it off the list.
                    let active_upstairs = self
                        .active_upstairs
                        .remove(&currently_active_upstairs_uuids[0])
                        .unwrap();

                    let mut work = active_upstairs.work.lock().await;

                    warn!(
                        self.log,
                        "Signaling to {:?} thread that {:?} is being \
                        promoted (read-write)",
                        active_upstairs.upstairs_connection,
                        upstairs_connection,
                    );

                    match futures::executor::block_on(
                        active_upstairs
                            .terminate_sender
                            .send(upstairs_connection),
                    ) {
                        Ok(_) => {}
                        Err(e) => {
                            /*
                             * It's possible the old thread died due to some
                             * connection error. In that case the
                             * receiver will have closed and
                             * the above send will fail.
                             */
                            error!(
                                self.log,
                                "Error while signaling to {:?} thread: {:?}",
                                active_upstairs.upstairs_connection,
                                e,
                            );
                        }
                    }

                    // Note: in the future, differentiate between new upstairs
                    // connecting vs same upstairs reconnecting here.
                    //
                    // Clear out active jobs, the last flush, and completed
                    // information, as that will not be valid any longer.
                    //
                    // TODO: Really work through this error case
                    if work.active.keys().len() > 0 {
                        warn!(
                            self.log,
                            "Crucible Downstairs promoting {:?} to active, \
                            discarding {} jobs",
                            upstairs_connection,
                            work.active.keys().len()
                        );
                    }

                    // In the future, we may decide there is some way to
                    // continue working on outstanding jobs, or a way to merge.
                    // But for now, we just throw out what we have and let the
                    // upstairs resend anything to us that it did not get an ACK
                    // for.
                    work.clear();

                    // Insert or replace the session

                    self.active_upstairs.insert(
                        upstairs_connection.upstairs_id,
                        ActiveUpstairs {
                            upstairs_connection,
                            work: Mutex::new(Work::new()),
                            terminate_sender: tx,
                        },
                    );

                    assert_eq!(self.active_upstairs.len(), 1);

                    // Re-open any closed extents
                    self.region.reopen_all_extents()?;

                    info!(
                        self.log,
                        "{:?} is now active (read-write)", upstairs_connection,
                    );

                    Ok(())
                }

                _ => {
                    // Panic - we shouldn't be running with more than one
                    // active read-write Upstairs
                    panic!(
                        "More than one currently active upstairs! {:?}",
                        currently_active_upstairs_uuids,
                    );
                }
            }
        }
    }

    fn is_active(&mut self, connection: UpstairsConnection) -> bool {
        let uuid = connection.upstairs_id;
        if let Some(active_upstairs) = self.active_upstairs.get(&uuid) {
            active_upstairs.upstairs_connection == connection
        } else {
            false
        }
    }

    fn active_upstairs(&mut self) -> Vec<UpstairsConnection> {
        self.active_upstairs
            .values()
            .map(|x| x.upstairs_connection)
            .collect()
    }

    async fn clear_active(
        &mut self,
        upstairs_connection: UpstairsConnection,
    ) -> Result<()> {
        let mut work = self.work_lock(upstairs_connection).await?;
        work.clear();
        drop(work);

        self.active_upstairs
            .remove(&upstairs_connection.upstairs_id);

        Ok(())
    }
}

/*
 * The structure that tracks downstairs work in progress
 */
#[derive(Debug, Default)]
pub struct Work {
    active: HashMap<u64, DownstairsWork>,
    outstanding_deps: HashMap<u64, usize>,

    /*
     * We have to keep track of all IOs that have been issued since
     * our last flush, as that is how we make sure dependencies are
     * respected. The last_flush is the downstairs job ID number (ds_id
     * typically) for the most recent flush.
     */
    last_flush: u64,
    completed: Vec<u64>,
}

#[derive(Debug, Clone)]
struct DownstairsWork {
    upstairs_connection: UpstairsConnection,
    ds_id: u64,
    work: IOop,
    state: WorkState,
}

impl Work {
    fn new() -> Self {
        Work {
            active: HashMap::new(),
            outstanding_deps: HashMap::new(),
            last_flush: 0,
            completed: Vec::with_capacity(32),
        }
    }

    fn clear(&mut self) {
        self.active = HashMap::new();
        self.outstanding_deps = HashMap::new();
        self.last_flush = 0;
        self.completed = Vec::with_capacity(32);
    }

    fn jobs(&self) -> usize {
        self.active.len()
    }

    /**
     * Return a list of downstairs request IDs that are new or have
     * been waiting for other dependencies to finish.
     */
    fn new_work(&self, upstairs_connection: UpstairsConnection) -> Vec<u64> {
        let mut result = Vec::with_capacity(self.active.len());

        for job in self.active.values() {
            if job.upstairs_connection != upstairs_connection {
                panic!("Old Upstairs Job in new_work!");
            }

            if job.state == WorkState::New || job.state == WorkState::DepWait {
                result.push(job.ds_id);
            }
        }

        result.sort_unstable();

        result
    }

    fn add_work(&mut self, ds_id: u64, dsw: DownstairsWork) {
        self.active.insert(ds_id, dsw);
    }

    #[cfg(test)]
    fn get_job(&mut self, ds_id: u64) -> DownstairsWork {
        self.active.get(&ds_id).unwrap().clone()
    }

    /**
     * If the requested job is still new, and the dependencies are all met,
     * return the job ID and the upstairs UUID, moving the state of the
     * job as InProgress.
     *
     * If this job is not new, then just return none.  This can be okay as
     * we build or work list with the new_work fn above, but we drop and
     * re-aquire the Work mutex and things can change.
     */
    fn in_progress(
        &mut self,
        ds_id: u64,
        log: Logger,
    ) -> Option<(u64, UpstairsConnection)> {
        /*
         * Once we support multiple threads, we can obtain a ds_id that
         * looked valid when we made a list of jobs, but something
         * else moved that job along and now it no longer exists.  We
         * need to handle that case correctly.
         */
        if let Some(job) = self.active.get_mut(&ds_id) {
            if job.state == WorkState::New || job.state == WorkState::DepWait {
                /*
                 * Before we can make this in_progress, we have to, while
                 * holding this locked, check the dep list if there is one
                 * and make sure all dependencies are completed.
                 */
                let dep_list = job.work.deps();

                /*
                 * See which of our dependencies are met.
                 * XXX Make this better/faster by removing the ones that
                 * are met, so next lap we don't have to check again?  There
                 * may be some debug value to knowing what the dep list was,
                 * so consider that before making this faster.
                 */
                let mut deps_outstanding: Vec<u64> =
                    Vec::with_capacity(dep_list.len());

                for dep in dep_list.iter() {
                    if dep <= &self.last_flush {
                        continue;
                    }

                    if !self.completed.contains(dep) {
                        deps_outstanding.push(*dep);
                    }
                }

                if !deps_outstanding.is_empty() {
                    let print = if let Some(existing_outstanding_deps) =
                        self.outstanding_deps.get(&ds_id)
                    {
                        *existing_outstanding_deps != deps_outstanding.len()
                    } else {
                        false
                    };

                    if print {
                        warn!(
                            log,
                            "{} job {} for connection {:?} waiting on {} deps",
                            ds_id,
                            match &job.work {
                                IOop::Write {
                                    dependencies: _,
                                    writes: _,
                                } => "Write",
                                IOop::WriteUnwritten {
                                    dependencies: _,
                                    writes: _,
                                } => "WriteUnwritten",
                                IOop::Flush {
                                    dependencies: _,
                                    flush_number: _flush_number,
                                    gen_number: _gen_number,
                                    snapshot_details: _,
                                } => "Flush",
                                IOop::Read {
                                    dependencies: _,
                                    requests: _,
                                } => "Read",
                            },
                            job.upstairs_connection,
                            deps_outstanding.len(),
                        );
                    }

                    let _ = self
                        .outstanding_deps
                        .insert(ds_id, deps_outstanding.len());

                    /*
                     * If we got here, then the dep is not met.
                     * Set DepWait if not already set.
                     */
                    if job.state == WorkState::New {
                        job.state = WorkState::DepWait;
                    }

                    return None;
                }

                /*
                 * We had no dependencies, or they are all completed, we
                 * can go ahead and work on this job.
                 */
                job.state = WorkState::InProgress;

                Some((job.ds_id, job.upstairs_connection))
            } else {
                /*
                 * job id is not new, we can't run it.
                 */
                None
            }
        } else {
            /*
             * XXX If another upstairs took over, a job ID could be
             * invalid.  Check here to verify that this set of
             * downstairs tasks is no longer active.
             */
            warn!(log, "This ID is no longer a valid job id");
            None
        }
    }

    // Return a job that's ready to have the work done
    async fn get_ready_job(&mut self, job_id: u64) -> Option<DownstairsWork> {
        match self.active.get(&job_id) {
            Some(job) => {
                assert_eq!(job.state, WorkState::InProgress);
                assert_eq!(job_id, job.ds_id);

                // validate that deps are done
                let dep_list = job.work.deps();
                for dep in dep_list {
                    let last_flush_satisfied = dep <= &self.last_flush;
                    let complete_satisfied = self.completed.contains(dep);

                    assert!(last_flush_satisfied || complete_satisfied);
                }

                Some(job.clone())
            }

            None => {
                /*
                 * This branch occurs when another Upstairs has promoted
                 * itself to active, causing active work to
                 * be cleared (in promote_to_active).
                 *
                 * If this has happened, work.completed and work.last_flush
                 * have also been reset. Do nothing here,
                 * especially since the Upstairs has already
                 * been notified.
                 */
                None
            }
        }
    }
}

/*
 * We may not need Done or Error.  At the moment all we actually look
 * at is New or InProgress.
 */
#[derive(Debug, Clone, PartialEq)]
pub enum WorkState {
    New,
    DepWait,
    InProgress,
    Done,
    Error,
}

impl fmt::Display for WorkState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkState::New => {
                write!(f, " New")
            }
            WorkState::DepWait => {
                write!(f, "DepW")
            }
            WorkState::InProgress => {
                write!(f, "In P")
            }
            WorkState::Done => {
                write!(f, "Done")
            }
            WorkState::Error => {
                write!(f, " Err")
            }
        }
    }
}

#[allow(clippy::large_enum_variant)]
enum WrappedStream {
    Http(tokio::net::TcpStream),
    Https(tokio_rustls::server::TlsStream<tokio::net::TcpStream>),
}

pub fn create_region(
    block_size: u64,
    data: PathBuf,
    extent_size: u64,
    extent_count: u64,
    uuid: Uuid,
    encrypted: bool,
    log: Logger,
) -> Result<Region> {
    /*
     * Create the region options, then the region.
     */
    let mut region_options: crucible_common::RegionOptions = Default::default();
    region_options.set_block_size(block_size);
    region_options
        .set_extent_size(Block::new(extent_size, block_size.trailing_zeros()));
    region_options.set_uuid(uuid);
    region_options.set_encrypted(encrypted);

    let mut region = Region::create(&data, region_options, log)?;
    region.extend(extent_count as u32)?;

    Ok(region)
}

// Build the downstairs struct given a region directory and some additional
// needed information.  If a logger is passed in, we will use that, otherwise
// a logger will be created.
pub fn build_downstairs_for_region(
    data: &Path,
    lossy: bool,
    return_errors: bool,
    read_only: bool,
    log_request: Option<Logger>,
) -> Result<Arc<Mutex<Downstairs>>> {
    let log = match log_request {
        Some(log) => log,
        None => {
            // Register DTrace, and setup slog logging to use it.
            register_probes().unwrap();
            let decorator = slog_term::TermDecorator::new().build();
            let drain = slog_term::FullFormat::new(decorator)
                .build()
                .filter_level(slog::Level::Info)
                .fuse();
            let drain = slog_async::Async::new(drain).build().fuse();
            let (drain, registration) = with_drain(drain);
            if let ProbeRegistration::Failed(ref e) = registration {
                panic!("Failed to register probes: {:#?}", e);
            }
            Logger::root(drain.fuse(), o!())
        }
    };
    let region =
        Region::open(&data, Default::default(), true, read_only, &log)?;

    info!(log, "UUID: {:?}", region.def().uuid());
    info!(
        log,
        "Blocks per extent:{} Total Extents: {}",
        region.def().extent_size().value,
        region.def().extent_count(),
    );

    let encrypted = region.encrypted();

    Ok(Arc::new(Mutex::new(Downstairs::new(
        region,
        lossy,
        return_errors,
        read_only,
        encrypted,
        log,
    ))))
}

/// Returns Ok if everything spawned ok, Err otherwise
///
/// Return Ok(main task join handle) if all the necessary tasks spawned
/// successfully, and Err otherwise.
#[allow(clippy::too_many_arguments)]
pub async fn start_downstairs(
    d: Arc<Mutex<Downstairs>>,
    address: IpAddr,
    oximeter: Option<SocketAddr>,
    port: u16,
    rport: u16,
    cert_pem: Option<String>,
    key_pem: Option<String>,
    root_cert_pem: Option<String>,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    if let Some(oximeter) = oximeter {
        let dssw = d.lock().await;
        let dss = dssw.dss.clone();
        let log = dssw.log.new(o!("task" => "oximeter".to_string()));

        tokio::spawn(async move {
            let new_address = match address {
                IpAddr::V4(ipv4) => {
                    SocketAddr::new(std::net::IpAddr::V4(ipv4), 0)
                }
                IpAddr::V6(ipv6) => {
                    SocketAddr::new(std::net::IpAddr::V6(ipv6), 0)
                }
            };

            if let Err(e) =
                stats::ox_stats(dss, oximeter, new_address, &log).await
            {
                error!(log, "ERROR: oximeter failed: {:?}", e);
            } else {
                warn!(log, "OK: oximeter all done");
            }
        });
    }

    // Setup a log for this task
    let log = d.lock().await.log.new(o!("task" => "main".to_string()));

    let listen_on = match address {
        IpAddr::V4(ipv4) => SocketAddr::new(std::net::IpAddr::V4(ipv4), port),
        IpAddr::V6(ipv6) => SocketAddr::new(std::net::IpAddr::V6(ipv6), port),
    };

    // Establish a listen server on the port.
    let listener = TcpListener::bind(&listen_on).await?;
    let local_addr = listener.local_addr()?;
    {
        let mut ds = d.lock().await;
        ds.address = Some(local_addr);
    }
    info!(log, "Using address: {:?}", local_addr);

    let repair_address = match address {
        IpAddr::V4(ipv4) => SocketAddr::new(std::net::IpAddr::V4(ipv4), rport),
        IpAddr::V6(ipv6) => SocketAddr::new(std::net::IpAddr::V6(ipv6), rport),
    };

    let dss = d.clone();
    let repair_log = d.lock().await.log.new(o!("task" => "repair".to_string()));

    let repair_listener =
        match repair::repair_main(&dss, repair_address, &repair_log).await {
            Err(e) => {
                // TODO tear down other things if repair server can't be
                // started?
                bail!("got {:?} from repair main", e);
            }

            Ok(socket_addr) => socket_addr,
        };

    {
        let mut ds = d.lock().await;
        ds.repair_address = Some(repair_listener);
    }
    info!(log, "Using repair address: {:?}", repair_listener);

    // Optionally require SSL connections
    let ssl_acceptor = if let Some(cert_pem_path) = cert_pem {
        let key_pem_path = key_pem.unwrap();
        let root_cert_pem_path = root_cert_pem.unwrap();

        let context = crucible_common::x509::TLSContext::from_paths(
            &cert_pem_path,
            &key_pem_path,
            &root_cert_pem_path,
        )?;

        let config = context.get_server_config()?;

        info!(log, "Configured SSL acceptor");

        Some(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
    } else {
        // unencrypted
        info!(log, "No SSL acceptor configured");
        None
    };

    let join_handle = tokio::spawn(async move {
        /*
         * We now loop listening for a connection from the Upstairs.
         * When we get one, we then spawn the proc() function to handle
         * it and wait for another connection. Downstairs can handle
         * multiple Upstairs connecting but only one active one.
         */
        info!(log, "listening on {}", listen_on);
        loop {
            let (sock, raddr) = listener.accept().await?;

            let stream: WrappedStream = if let Some(ssl_acceptor) =
                &ssl_acceptor
            {
                let ssl_acceptor = ssl_acceptor.clone();
                WrappedStream::Https(match ssl_acceptor.accept(sock).await {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(
                            log,
                            "rejecting connection from {:?}: {:?}", raddr, e,
                        );
                        continue;
                    }
                })
            } else {
                WrappedStream::Http(sock)
            };

            info!(log, "accepted connection from {:?}", raddr);
            {
                /*
                 * Add one to the counter every time we have a connection
                 * from an upstairs
                 */
                let mut ds = d.lock().await;
                ds.dss.add_connection().await;
            }

            let mut dd = d.clone();

            tokio::spawn(async move {
                if let Err(e) = proc_stream(&mut dd, stream).await {
                    error!(
                        dd.lock().await.log,
                        "connection({}): {:?}", raddr, e
                    );
                } else {
                    info!(
                        dd.lock().await.log,
                        "connection({}): all done", raddr
                    );
                }
            });
        }
    });

    Ok(join_handle)
}

#[cfg(test)]
mod test {
    use super::*;
    use rand_chacha::ChaCha20Rng;
    use tempfile::tempdir;
    use tokio::sync::mpsc::error::TryRecvError;

    // Create a simple logger
    fn csl() -> Logger {
        let plain = slog_term::PlainSyncDecorator::new(std::io::stdout());
        Logger::root(slog_term::FullFormat::new(plain).build().fuse(), o!())
    }

    fn add_work(
        work: &mut Work,
        upstairs_connection: UpstairsConnection,
        ds_id: u64,
        deps: Vec<u64>,
        is_flush: bool,
    ) {
        work.add_work(
            ds_id,
            DownstairsWork {
                upstairs_connection,
                ds_id,
                work: if is_flush {
                    IOop::Flush {
                        dependencies: deps,
                        flush_number: 10,
                        gen_number: 0,
                        snapshot_details: None,
                    }
                } else {
                    IOop::Read {
                        dependencies: deps,
                        requests: vec![ReadRequest {
                            eid: 1,
                            offset: Block::new_512(1),
                        }],
                    }
                },
                state: WorkState::New,
            },
        );
    }

    fn add_work_rf(
        work: &mut Work,
        upstairs_connection: UpstairsConnection,
        ds_id: u64,
        deps: Vec<u64>,
    ) {
        work.add_work(
            ds_id,
            DownstairsWork {
                upstairs_connection,
                ds_id,
                work: IOop::WriteUnwritten {
                    dependencies: deps,
                    writes: Vec::with_capacity(1),
                },
                state: WorkState::New,
            },
        );
    }

    fn complete(work: &mut Work, ds_id: u64) {
        let is_flush = {
            let job = work.active.get(&ds_id).unwrap();

            // validate that deps are done
            let dep_list = job.work.deps();
            for dep in dep_list {
                let last_flush_satisfied = dep <= &work.last_flush;
                let complete_satisfied = work.completed.contains(dep);

                assert!(last_flush_satisfied || complete_satisfied);
            }

            matches!(
                job.work,
                IOop::Flush {
                    dependencies: _,
                    flush_number: _,
                    gen_number: _,
                    snapshot_details: _,
                }
            )
        };

        let _ = work.active.remove(&ds_id);

        if is_flush {
            work.last_flush = ds_id;
            work.completed = Vec::with_capacity(32);
        } else {
            work.completed.push(ds_id);
        }
    }

    fn test_push_next_jobs(
        work: &mut Work,
        upstairs_connection: UpstairsConnection,
    ) -> Vec<u64> {
        let mut jobs = vec![];
        let mut new_work = work.new_work(upstairs_connection);

        new_work.sort_unstable();

        for new_id in new_work.iter() {
            let job = work.in_progress(*new_id, csl());
            match job {
                Some(job) => {
                    jobs.push(job.0);
                }
                None => {
                    continue;
                }
            }
        }

        for job in &jobs {
            assert_eq!(
                work.active.get(job).unwrap().state,
                WorkState::InProgress
            );
        }

        jobs
    }

    fn test_do_work(work: &mut Work, jobs: Vec<u64>) {
        for job_id in jobs {
            complete(work, job_id);
        }
    }

    #[test]
    fn you_had_one_job() {
        let mut work = Work::default();
        let upstairs_connection = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 0,
        };

        add_work(&mut work, upstairs_connection, 1000, vec![], false);

        assert_eq!(work.new_work(upstairs_connection), vec![1000]);

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1000]);

        test_do_work(&mut work, next_jobs);

        assert_eq!(work.completed, vec![1000]);

        assert!(test_push_next_jobs(&mut work, upstairs_connection).is_empty());
    }

    #[tokio::test]
    async fn test_simple_read() -> Result<()> {
        // Test region create and a read of one block.
        let block_size: u64 = 512;
        let extent_size = 4;

        // create region
        let mut region_options: crucible_common::RegionOptions =
            Default::default();
        region_options.set_block_size(block_size);
        region_options.set_extent_size(Block::new(
            extent_size,
            block_size.trailing_zeros(),
        ));
        region_options.set_uuid(Uuid::new_v4());

        let dir = tempdir()?;
        mkdir_for_file(dir.path())?;

        let mut region = Region::create(&dir, region_options, csl())?;
        region.extend(2)?;

        let path_dir = dir.as_ref().to_path_buf();
        let ads = build_downstairs_for_region(
            &path_dir,
            false,
            false,
            false,
            Some(csl()),
        )?;

        // This happens in proc() function.
        let upstairs_connection = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 10,
        };

        // For the other_active_upstairs, unused.
        let (_tx, mut _rx) = channel(1);
        let tx = Arc::new(_tx);

        let mut ds = ads.lock().await;
        ds.promote_to_active(upstairs_connection, tx.clone())
            .await?;

        let rio = IOop::Read {
            dependencies: Vec::new(),
            requests: vec![ReadRequest {
                eid: 0,
                offset: Block::new_512(1),
            }],
        };
        ds.add_work(upstairs_connection, 1000, rio).await?;

        let deps = vec![1000];
        let rio = IOop::Read {
            dependencies: deps,
            requests: vec![ReadRequest {
                eid: 1,
                offset: Block::new_512(1),
            }],
        };
        ds.add_work(upstairs_connection, 1001, rio).await?;

        show_work(&mut ds).await;

        // Now we mimic what happens in the do_work_task()
        let new_work = ds.new_work(upstairs_connection).await.unwrap();
        println!("Got new work: {:?}", new_work);
        assert_eq!(new_work.len(), 2);

        for id in new_work.iter() {
            let ip_id =
                ds.in_progress(upstairs_connection, *id).await?.unwrap();
            assert_eq!(ip_id, *id);
            println!("Do IOop {}", *id);
            let m = ds.do_work(upstairs_connection, *id).await?.unwrap();
            println!("Got m: {:?}", m);
            ds.complete_work(upstairs_connection, *id, m).await?;
        }
        show_work(&mut ds).await;
        Ok(())
    }

    #[test]
    fn jobs_write_unwritten() {
        // Verify WriteUnwritten jobs move through the queue
        let mut work = Work::default();
        let upstairs_connection = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 0,
        };

        add_work_rf(&mut work, upstairs_connection, 1000, vec![]);

        assert_eq!(work.new_work(upstairs_connection), vec![1000]);

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1000]);

        test_do_work(&mut work, next_jobs);

        assert_eq!(work.completed, vec![1000]);

        assert!(test_push_next_jobs(&mut work, upstairs_connection).is_empty());
    }

    #[test]
    fn jobs_independent() {
        let mut work = Work::default();
        let upstairs_connection = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 0,
        };

        // Add two independent jobs
        add_work(&mut work, upstairs_connection, 1000, vec![], false);
        add_work(&mut work, upstairs_connection, 1001, vec![], false);

        // new_work returns all new jobs
        assert_eq!(work.new_work(upstairs_connection), vec![1000, 1001]);

        // should push both, they're independent
        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1000, 1001]);

        // new work returns only jobs in new or dep wait
        assert!(work.new_work(upstairs_connection).is_empty());

        // do work
        test_do_work(&mut work, next_jobs);

        assert_eq!(work.completed, vec![1000, 1001]);

        assert!(test_push_next_jobs(&mut work, upstairs_connection).is_empty());
    }

    #[test]
    fn unblock_job() {
        let mut work = Work::default();
        let upstairs_connection = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 0,
        };

        // Add two jobs, one blocked on another
        add_work(&mut work, upstairs_connection, 1000, vec![], false);
        add_work(&mut work, upstairs_connection, 1001, vec![1000], false);

        // new_work returns all new or dep wait jobs
        assert_eq!(work.new_work(upstairs_connection), vec![1000, 1001]);

        // only one is ready to run
        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1000]);

        // new_work returns all new or dep wait jobs
        assert_eq!(work.new_work(upstairs_connection), vec![1001]);

        test_do_work(&mut work, next_jobs);

        assert_eq!(work.completed, vec![1000]);

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1001]);
    }

    #[test]
    fn unblock_job_chain() {
        let mut work = Work::default();
        let upstairs_connection = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 0,
        };

        // Add three jobs all blocked on each other in a chain
        add_work(&mut work, upstairs_connection, 1000, vec![], false);
        add_work(&mut work, upstairs_connection, 1001, vec![1000], false);
        add_work(
            &mut work,
            upstairs_connection,
            1002,
            vec![1000, 1001],
            false,
        );

        // new_work returns all new or dep wait jobs
        assert_eq!(work.new_work(upstairs_connection), vec![1000, 1001, 1002]);

        // only one is ready to run at a time

        assert!(work.completed.is_empty());
        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1000]);
        assert_eq!(work.new_work(upstairs_connection), vec![1001, 1002]);

        test_do_work(&mut work, next_jobs);

        assert_eq!(work.completed, vec![1000]);
        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1001]);
        assert_eq!(work.new_work(upstairs_connection), vec![1002]);

        test_do_work(&mut work, next_jobs);

        assert_eq!(work.completed, vec![1000, 1001]);
        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1002]);
        assert!(work.new_work(upstairs_connection).is_empty());

        test_do_work(&mut work, next_jobs);

        assert_eq!(work.completed, vec![1000, 1001, 1002]);
    }

    #[test]
    fn unblock_job_chain_first_is_flush() {
        let mut work = Work::default();
        let upstairs_connection = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 0,
        };

        // Add three jobs all blocked on each other in a chain, first is flush
        add_work(&mut work, upstairs_connection, 1000, vec![], true);
        add_work(&mut work, upstairs_connection, 1001, vec![1000], false);
        add_work(
            &mut work,
            upstairs_connection,
            1002,
            vec![1000, 1001],
            false,
        );

        // new_work returns all new or dep wait jobs
        assert_eq!(work.new_work(upstairs_connection), vec![1000, 1001, 1002]);

        // only one is ready to run at a time

        assert!(work.completed.is_empty());
        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1000]);
        assert_eq!(work.new_work(upstairs_connection), vec![1001, 1002]);

        test_do_work(&mut work, next_jobs);

        assert_eq!(work.last_flush, 1000);
        assert!(work.completed.is_empty());
        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1001]);
        assert_eq!(work.new_work(upstairs_connection), vec![1002]);

        test_do_work(&mut work, next_jobs);

        assert_eq!(work.last_flush, 1000);
        assert_eq!(work.completed, vec![1001]);
        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1002]);
        assert!(work.new_work(upstairs_connection).is_empty());

        test_do_work(&mut work, next_jobs);

        assert_eq!(work.last_flush, 1000);
        assert_eq!(work.completed, vec![1001, 1002]);
    }

    #[test]
    fn unblock_job_chain_second_is_flush() {
        let mut work = Work::default();
        let upstairs_connection = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 0,
        };

        // Add three jobs all blocked on each other in a chain, second is flush
        add_work(&mut work, upstairs_connection, 1000, vec![], false);
        add_work(&mut work, upstairs_connection, 1001, vec![1000], true);
        add_work(
            &mut work,
            upstairs_connection,
            1002,
            vec![1000, 1001],
            false,
        );

        // new_work returns all new or dep wait jobs
        assert_eq!(work.new_work(upstairs_connection), vec![1000, 1001, 1002]);

        // only one is ready to run at a time

        assert!(work.completed.is_empty());
        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1000]);
        assert_eq!(work.new_work(upstairs_connection), vec![1001, 1002]);

        test_do_work(&mut work, next_jobs);

        assert_eq!(work.completed, vec![1000]);
        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1001]);
        assert_eq!(work.new_work(upstairs_connection), vec![1002]);

        test_do_work(&mut work, next_jobs);

        assert_eq!(work.last_flush, 1001);
        assert!(work.completed.is_empty());
        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1002]);
        assert!(work.new_work(upstairs_connection).is_empty());

        test_do_work(&mut work, next_jobs);

        assert_eq!(work.last_flush, 1001);
        assert_eq!(work.completed, vec![1002]);
    }

    #[test]
    fn unblock_job_upstairs_sends_big_deps() {
        let mut work = Work::default();
        let upstairs_connection = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 0,
        };

        // Add three jobs all blocked on each other
        add_work(&mut work, upstairs_connection, 1000, vec![], false);
        add_work(&mut work, upstairs_connection, 1001, vec![1000], false);
        add_work(&mut work, upstairs_connection, 1002, vec![1000, 1001], true);

        // Downstairs is really fast!
        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1000]);
        test_do_work(&mut work, next_jobs);

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1001]);
        test_do_work(&mut work, next_jobs);

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1002]);
        test_do_work(&mut work, next_jobs);

        assert_eq!(work.last_flush, 1002);
        assert!(work.completed.is_empty());

        // Upstairs sends a job with these three in deps, not knowing Downstairs
        // has done the jobs already
        add_work(
            &mut work,
            upstairs_connection,
            1003,
            vec![1000, 1001, 1002],
            false,
        );
        add_work(
            &mut work,
            upstairs_connection,
            1004,
            vec![1000, 1001, 1002, 1003],
            false,
        );

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1003]);
        test_do_work(&mut work, next_jobs);

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1004]);
        test_do_work(&mut work, next_jobs);

        assert_eq!(work.last_flush, 1002);
        assert_eq!(work.completed, vec![1003, 1004]);
    }

    #[test]
    fn job_dep_not_satisfied() {
        let mut work = Work::default();
        let upstairs_connection = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 0,
        };

        // Add three jobs all blocked on each other
        add_work(&mut work, upstairs_connection, 1000, vec![], false);
        add_work(&mut work, upstairs_connection, 1001, vec![1000], false);
        add_work(&mut work, upstairs_connection, 1002, vec![1000, 1001], true);

        // Add one that can't run yet
        add_work(&mut work, upstairs_connection, 1003, vec![2000], false);

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1000]);
        test_do_work(&mut work, next_jobs);

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1001]);
        test_do_work(&mut work, next_jobs);

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1002]);
        test_do_work(&mut work, next_jobs);

        assert_eq!(work.last_flush, 1002);
        assert!(work.completed.is_empty());

        assert_eq!(work.new_work(upstairs_connection), vec![1003]);
        assert_eq!(work.active.get(&1003).unwrap().state, WorkState::DepWait);
    }

    #[test]
    fn two_job_chains() {
        let mut work = Work::default();
        let upstairs_connection = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 0,
        };

        // Add three jobs all blocked on each other
        add_work(&mut work, upstairs_connection, 1000, vec![], false);
        add_work(&mut work, upstairs_connection, 1001, vec![1000], false);
        add_work(
            &mut work,
            upstairs_connection,
            1002,
            vec![1000, 1001],
            false,
        );

        // Add another set of jobs blocked on each other
        add_work(&mut work, upstairs_connection, 2000, vec![], false);
        add_work(&mut work, upstairs_connection, 2001, vec![2000], false);
        add_work(&mut work, upstairs_connection, 2002, vec![2000, 2001], true);

        // should do each chain in sequence
        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1000, 2000]);
        test_do_work(&mut work, next_jobs);
        assert_eq!(work.completed, vec![1000, 2000]);

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1001, 2001]);
        test_do_work(&mut work, next_jobs);
        assert_eq!(work.completed, vec![1000, 2000, 1001, 2001]);

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1002, 2002]);
        test_do_work(&mut work, next_jobs);

        assert_eq!(work.last_flush, 2002);
        assert!(work.completed.is_empty());
    }

    #[test]
    fn out_of_order_arrives_after_first_push_next_jobs() {
        /*
         * Test that jobs arriving out of order still complete.
         */
        let mut work = Work::default();
        let upstairs_connection = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 0,
        };

        // Add three jobs all blocked on each other (missing 1002)
        add_work(&mut work, upstairs_connection, 1000, vec![], false);
        add_work(&mut work, upstairs_connection, 1001, vec![1000], false);
        add_work(
            &mut work,
            upstairs_connection,
            1003,
            vec![1000, 1001, 1002],
            false,
        );

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1000]);

        add_work(
            &mut work,
            upstairs_connection,
            1002,
            vec![1000, 1001],
            false,
        );

        test_do_work(&mut work, next_jobs);

        assert_eq!(work.completed, vec![1000]);

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1001]);
        test_do_work(&mut work, next_jobs);

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1002]);
        test_do_work(&mut work, next_jobs);

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1003]);
        test_do_work(&mut work, next_jobs);

        assert_eq!(work.completed, vec![1000, 1001, 1002, 1003]);
    }

    #[test]
    fn out_of_order_arrives_after_first_do_work() {
        /*
         * Test that jobs arriving out of order still complete.
         */
        let mut work = Work::default();
        let upstairs_connection = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 0,
        };

        // Add three jobs all blocked on each other (missing 1002)
        add_work(&mut work, upstairs_connection, 1000, vec![], false);
        add_work(&mut work, upstairs_connection, 1001, vec![1000], false);
        add_work(
            &mut work,
            upstairs_connection,
            1003,
            vec![1000, 1001, 1002],
            false,
        );

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1000]);

        test_do_work(&mut work, next_jobs);

        add_work(
            &mut work,
            upstairs_connection,
            1002,
            vec![1000, 1001],
            false,
        );

        assert_eq!(work.completed, vec![1000]);

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1001]);
        test_do_work(&mut work, next_jobs);

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1002]);
        test_do_work(&mut work, next_jobs);

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1003]);
        test_do_work(&mut work, next_jobs);

        assert_eq!(work.completed, vec![1000, 1001, 1002, 1003]);
    }

    #[test]
    fn out_of_order_arrives_after_1001_completes() {
        /*
         * Test that jobs arriving out of order still complete.
         */
        let mut work = Work::default();
        let upstairs_connection = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 0,
        };

        // Add three jobs all blocked on each other (missing 1002)
        add_work(&mut work, upstairs_connection, 1000, vec![], false);
        add_work(&mut work, upstairs_connection, 1001, vec![1000], false);
        add_work(
            &mut work,
            upstairs_connection,
            1003,
            vec![1000, 1001, 1002],
            false,
        );

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1000]);

        test_do_work(&mut work, next_jobs);

        assert_eq!(work.completed, vec![1000]);

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1001]);
        test_do_work(&mut work, next_jobs);

        // can't run anything, dep not satisfied
        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert!(next_jobs.is_empty());
        test_do_work(&mut work, next_jobs);

        add_work(
            &mut work,
            upstairs_connection,
            1002,
            vec![1000, 1001],
            false,
        );

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1002]);
        test_do_work(&mut work, next_jobs);

        let next_jobs = test_push_next_jobs(&mut work, upstairs_connection);
        assert_eq!(next_jobs, vec![1003]);
        test_do_work(&mut work, next_jobs);

        assert_eq!(work.completed, vec![1000, 1001, 1002, 1003]);
    }

    #[test]
    fn import_test_basic() -> Result<()> {
        /*
         * import + export test where data matches region size
         */

        let block_size: u64 = 512;
        let extent_size = 10;

        // create region

        let mut region_options: crucible_common::RegionOptions =
            Default::default();
        region_options.set_block_size(block_size);
        region_options.set_extent_size(Block::new(
            extent_size,
            block_size.trailing_zeros(),
        ));
        region_options.set_uuid(Uuid::new_v4());

        let dir = tempdir()?;
        mkdir_for_file(dir.path())?;

        let mut region = Region::create(&dir, region_options, csl())?;
        region.extend(10)?;

        // create random file

        let total_bytes = region.def().total_size();
        let mut random_data = vec![0; total_bytes as usize];
        random_data.resize(total_bytes as usize, 0);

        let mut rng = ChaCha20Rng::from_entropy();
        rng.fill_bytes(&mut random_data);

        // write random_data to file

        let tempdir = tempdir()?;
        mkdir_for_file(tempdir.path())?;

        let random_file_path = tempdir.path().join("random_data");
        let mut random_file = File::create(&random_file_path)?;
        random_file.write_all(&random_data[..])?;

        // import random_data to the region

        downstairs_import(&mut region, &random_file_path)?;
        region.region_flush(1, 1, &None, 0)?;

        // export region to another file

        let export_path = tempdir.path().join("exported_data");
        downstairs_export(
            &mut region,
            &export_path,
            0,
            total_bytes / block_size,
        )?;

        // compare files

        let expected = std::fs::read(random_file_path)?;
        let actual = std::fs::read(export_path)?;

        assert_eq!(expected, actual);

        Ok(())
    }

    #[test]
    fn import_test_too_small() -> Result<()> {
        /*
         * import + export test where data is smaller than region size
         */
        let block_size: u64 = 512;
        let extent_size = 10;

        // create region

        let mut region_options: crucible_common::RegionOptions =
            Default::default();
        region_options.set_block_size(block_size);
        region_options.set_extent_size(Block::new(
            extent_size,
            block_size.trailing_zeros(),
        ));
        region_options.set_uuid(Uuid::new_v4());

        let dir = tempdir()?;
        mkdir_for_file(dir.path())?;

        let mut region = Region::create(&dir, region_options, csl())?;
        region.extend(10)?;

        // create random file (100 fewer bytes than region size)

        let total_bytes = region.def().total_size() - 100;
        let mut random_data = vec![0; total_bytes as usize];
        random_data.resize(total_bytes as usize, 0);

        let mut rng = ChaCha20Rng::from_entropy();
        rng.fill_bytes(&mut random_data);

        // write random_data to file

        let tempdir = tempdir()?;
        mkdir_for_file(tempdir.path())?;

        let random_file_path = tempdir.path().join("random_data");
        let mut random_file = File::create(&random_file_path)?;
        random_file.write_all(&random_data[..])?;

        // import random_data to the region

        downstairs_import(&mut region, &random_file_path)?;
        region.region_flush(1, 1, &None, 0)?;

        // export region to another file (note: 100 fewer bytes imported than
        // region size still means the whole region is exported)

        let export_path = tempdir.path().join("exported_data");
        let region_size = region.def().total_size();
        downstairs_export(
            &mut region,
            &export_path,
            0,
            region_size / block_size,
        )?;

        // compare files

        let expected = std::fs::read(random_file_path)?;
        let actual = std::fs::read(export_path)?;

        // assert what was imported is correct

        let total_bytes = total_bytes as usize;
        assert_eq!(expected, actual[0..total_bytes]);

        // assert the rest is zero padded

        let padding_size = actual.len() - total_bytes;
        assert_eq!(padding_size, 100);

        let mut padding = vec![0; padding_size];
        padding.resize(padding_size, 0);
        assert_eq!(actual[total_bytes..], padding);

        Ok(())
    }

    #[test]
    fn import_test_too_large() -> Result<()> {
        /*
         * import + export test where data is larger than region size
         */
        let block_size: u64 = 512;
        let extent_size = 10;

        // create region

        let mut region_options: crucible_common::RegionOptions =
            Default::default();
        region_options.set_block_size(block_size);
        region_options.set_extent_size(Block::new(
            extent_size,
            block_size.trailing_zeros(),
        ));
        region_options.set_uuid(Uuid::new_v4());

        let dir = tempdir()?;
        mkdir_for_file(dir.path())?;

        let mut region = Region::create(&dir, region_options, csl())?;
        region.extend(10)?;

        // create random file (100 more bytes than region size)

        let total_bytes = region.def().total_size() + 100;
        let mut random_data = vec![0; total_bytes as usize];
        random_data.resize(total_bytes as usize, 0);

        let mut rng = ChaCha20Rng::from_entropy();
        rng.fill_bytes(&mut random_data);

        // write random_data to file

        let tempdir = tempdir()?;
        mkdir_for_file(tempdir.path())?;

        let random_file_path = tempdir.path().join("random_data");
        let mut random_file = File::create(&random_file_path)?;
        random_file.write_all(&random_data[..])?;

        // import random_data to the region

        downstairs_import(&mut region, &random_file_path)?;
        region.region_flush(1, 1, &None, 0)?;

        // export region to another file (note: 100 more bytes will have caused
        // 10 more extents to be added, but someone running the export command
        // will use the number of blocks copied by the import command)
        assert_eq!(region.def().extent_count(), 11);

        let export_path = tempdir.path().join("exported_data");
        downstairs_export(
            &mut region,
            &export_path,
            0,
            total_bytes / block_size + 1,
        )?;

        // compare files

        let expected = std::fs::read(random_file_path)?;
        let actual = std::fs::read(export_path)?;

        // assert what was imported is correct

        let total_bytes = total_bytes as usize;
        assert_eq!(expected, actual[0..total_bytes]);

        // assert the rest is zero padded
        // the export only exported the extra block, not the extra extent
        let padding_in_extra_block: usize = 512 - 100;

        let mut padding = vec![0; padding_in_extra_block];
        padding.resize(padding_in_extra_block, 0);
        assert_eq!(actual[total_bytes..], padding);

        Ok(())
    }

    #[test]
    fn import_test_basic_read_blocks() -> Result<()> {
        /*
         * import + export test where data matches region size, and read the
         * blocks
         */
        let block_size: u64 = 512;
        let extent_size = 10;

        // create region

        let mut region_options: crucible_common::RegionOptions =
            Default::default();
        region_options.set_block_size(block_size);
        region_options.set_extent_size(Block::new(
            extent_size,
            block_size.trailing_zeros(),
        ));
        region_options.set_uuid(Uuid::new_v4());

        let dir = tempdir()?;
        mkdir_for_file(dir.path())?;

        let mut region = Region::create(&dir, region_options, csl())?;
        region.extend(10)?;

        // create random file

        let total_bytes = region.def().total_size();
        let mut random_data = vec![0u8; total_bytes as usize];
        random_data.resize(total_bytes as usize, 0u8);

        let mut rng = ChaCha20Rng::from_entropy();
        rng.fill_bytes(&mut random_data);

        // write random_data to file

        let tempdir = tempdir()?;
        mkdir_for_file(tempdir.path())?;

        let random_file_path = tempdir.path().join("random_data");
        let mut random_file = File::create(&random_file_path)?;
        random_file.write_all(&random_data[..])?;

        // import random_data to the region

        downstairs_import(&mut region, &random_file_path)?;
        region.region_flush(1, 1, &None, 0)?;

        // read block by block
        let mut read_data = Vec::with_capacity(total_bytes as usize);
        for eid in 0..region.def().extent_count() {
            for offset in 0..region.def().extent_size().value {
                let responses = region.region_read(
                    &[crucible_protocol::ReadRequest {
                        eid: eid.into(),
                        offset: Block::new_512(offset),
                    }],
                    0,
                )?;

                assert_eq!(responses.len(), 1);

                let response = &responses[0];
                assert_eq!(response.hashes().len(), 1);
                assert_eq!(
                    integrity_hash(&[&response.data[..]]),
                    response.hashes()[0],
                );

                read_data.extend_from_slice(&response.data[..]);
            }
        }

        assert_eq!(random_data, read_data);

        Ok(())
    }

    fn build_test_downstairs(
        read_only: bool,
    ) -> Result<Arc<Mutex<Downstairs>>> {
        let block_size: u64 = 512;
        let extent_size = 4;

        let mut region_options: crucible_common::RegionOptions =
            Default::default();
        region_options.set_block_size(block_size);
        region_options.set_extent_size(Block::new(
            extent_size,
            block_size.trailing_zeros(),
        ));
        region_options.set_uuid(Uuid::new_v4());

        let dir = tempdir()?;
        mkdir_for_file(dir.path())?;

        let mut region = Region::create(&dir, region_options, csl())?;
        region.extend(2)?;

        let path_dir = dir.as_ref().to_path_buf();

        build_downstairs_for_region(
            &path_dir,
            false, // lossy
            false, // return_errors
            read_only,
            Some(csl()),
        )
    }

    #[tokio::test]
    async fn test_promote_to_active_one_read_write() -> Result<()> {
        let ads = build_test_downstairs(false)?;

        let upstairs_connection = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 1,
        };

        let (_tx, mut _rx) = channel(1);
        let tx = Arc::new(_tx);

        let mut ds = ads.lock().await;
        ds.promote_to_active(upstairs_connection, tx).await?;

        assert_eq!(ds.active_upstairs().len(), 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_promote_to_active_one_read_only() -> Result<()> {
        let ads = build_test_downstairs(true)?;

        let upstairs_connection = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 1,
        };

        let (_tx, mut _rx) = channel(1);
        let tx = Arc::new(_tx);

        let mut ds = ads.lock().await;
        ds.promote_to_active(upstairs_connection, tx).await?;

        assert_eq!(ds.active_upstairs().len(), 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_promote_to_active_multi_read_write_different_uuid_same_gen(
    ) -> Result<()> {
        // Attempting to activate multiple read-write (where it's different
        // Upstairs) but with the same gen should be blocked
        let ads = build_test_downstairs(false)?;

        let upstairs_connection_1 = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 1,
        };

        let upstairs_connection_2 = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 1,
        };

        let (tx1, mut rx1) = channel(1);
        let tx1 = Arc::new(tx1);

        let (tx2, mut rx2) = channel(1);
        let tx2 = Arc::new(tx2);

        let mut ds = ads.lock().await;
        ds.promote_to_active(upstairs_connection_1, tx1).await?;

        assert_eq!(ds.active_upstairs().len(), 1);
        assert!(matches!(rx1.try_recv().err().unwrap(), TryRecvError::Empty));
        assert!(matches!(rx2.try_recv().err().unwrap(), TryRecvError::Empty));

        let res = ds.promote_to_active(upstairs_connection_2, tx2).await;
        assert!(res.is_err());

        assert!(matches!(rx1.try_recv().unwrap_err(), TryRecvError::Empty));
        assert!(matches!(
            rx2.try_recv().unwrap_err(),
            TryRecvError::Disconnected
        ));

        assert_eq!(ds.active_upstairs().len(), 1);

        // Original connection is still active.
        assert!(ds.is_active(upstairs_connection_1));
        // New connection was blocked.
        assert!(!ds.is_active(upstairs_connection_2));

        Ok(())
    }

    #[tokio::test]
    async fn test_promote_to_active_multi_read_write_different_uuid_lower_gen(
    ) -> Result<()> {
        // Attempting to activate multiple read-write (where it's different
        // Upstairs) but with a lower gen should be blocked.
        let ads = build_test_downstairs(false)?;

        let upstairs_connection_1 = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 2,
        };

        let upstairs_connection_2 = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 1,
        };

        let (tx1, mut rx1) = channel(1);
        let tx1 = Arc::new(tx1);

        let (tx2, mut rx2) = channel(1);
        let tx2 = Arc::new(tx2);

        let mut ds = ads.lock().await;
        println!("ds1: {:?}", ds);
        ds.promote_to_active(upstairs_connection_1, tx1).await?;
        println!("\nds2: {:?}\n", ds);

        assert_eq!(ds.active_upstairs().len(), 1);
        assert!(matches!(rx1.try_recv().err().unwrap(), TryRecvError::Empty));
        assert!(matches!(rx2.try_recv().err().unwrap(), TryRecvError::Empty));

        let res = ds.promote_to_active(upstairs_connection_2, tx2).await;
        assert!(res.is_err());

        assert!(matches!(rx1.try_recv().unwrap_err(), TryRecvError::Empty));
        assert!(matches!(
            rx2.try_recv().unwrap_err(),
            TryRecvError::Disconnected
        ));

        assert_eq!(ds.active_upstairs().len(), 1);

        // Original connection is still active.
        assert!(ds.is_active(upstairs_connection_1));
        // New connection was blocked.
        assert!(!ds.is_active(upstairs_connection_2));

        Ok(())
    }

    #[tokio::test]
    async fn test_promote_to_active_multi_read_write_same_uuid_same_gen(
    ) -> Result<()> {
        // Attempting to activate multiple read-write (where it's the same
        // Upstairs but a different session) will block the "new" connection
        // if it has the same generation number.
        let ads = build_test_downstairs(false)?;

        let upstairs_connection_1 = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 1,
        };

        let upstairs_connection_2 = UpstairsConnection {
            upstairs_id: upstairs_connection_1.upstairs_id,
            session_id: Uuid::new_v4(),
            gen: 1,
        };

        let (tx1, mut rx1) = channel(1);
        let tx1 = Arc::new(tx1);

        let (tx2, mut rx2) = channel(1);
        let tx2 = Arc::new(tx2);

        let mut ds = ads.lock().await;
        ds.promote_to_active(upstairs_connection_1, tx1).await?;

        assert_eq!(ds.active_upstairs().len(), 1);
        assert!(matches!(rx1.try_recv().err().unwrap(), TryRecvError::Empty));
        assert!(matches!(rx2.try_recv().err().unwrap(), TryRecvError::Empty));

        let res = ds.promote_to_active(upstairs_connection_2, tx2).await;
        assert!(res.is_err());

        assert!(matches!(rx1.try_recv().unwrap_err(), TryRecvError::Empty));
        assert!(matches!(
            rx2.try_recv().unwrap_err(),
            TryRecvError::Disconnected
        ));

        assert_eq!(ds.active_upstairs().len(), 1);

        assert!(ds.is_active(upstairs_connection_1));
        assert!(!ds.is_active(upstairs_connection_2));

        Ok(())
    }

    #[tokio::test]
    async fn test_promote_to_active_multi_read_write_same_uuid_larger_gen(
    ) -> Result<()> {
        // Attempting to activate multiple read-write where it's the same
        // Upstairs, but a different session, and with a larger generation
        // should allow the new connection to take over.
        let ads = build_test_downstairs(false)?;

        let upstairs_connection_1 = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 1,
        };

        let upstairs_connection_2 = UpstairsConnection {
            upstairs_id: upstairs_connection_1.upstairs_id,
            session_id: Uuid::new_v4(),
            gen: 2,
        };

        let (tx1, mut rx1) = channel(1);
        let tx1 = Arc::new(tx1);

        let (tx2, mut rx2) = channel(1);
        let tx2 = Arc::new(tx2);

        let mut ds = ads.lock().await;
        ds.promote_to_active(upstairs_connection_1, tx1).await?;

        assert_eq!(ds.active_upstairs().len(), 1);
        assert!(matches!(rx1.try_recv().err().unwrap(), TryRecvError::Empty));
        assert!(matches!(rx2.try_recv().err().unwrap(), TryRecvError::Empty));

        ds.promote_to_active(upstairs_connection_2, tx2).await?;
        assert_eq!(rx1.try_recv().unwrap(), upstairs_connection_2);
        assert!(matches!(rx2.try_recv().unwrap_err(), TryRecvError::Empty));

        assert_eq!(ds.active_upstairs().len(), 1);

        assert!(!ds.is_active(upstairs_connection_1));
        assert!(ds.is_active(upstairs_connection_2));

        Ok(())
    }

    #[tokio::test]
    async fn test_promote_to_active_multi_read_only_different_uuid(
    ) -> Result<()> {
        // Activating multiple read-only with different Upstairs UUIDs should
        // work.
        let ads = build_test_downstairs(true)?;

        let upstairs_connection_1 = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 1,
        };

        let upstairs_connection_2 = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 1,
        };

        let (tx1, mut rx1) = channel(1);
        let tx1 = Arc::new(tx1);

        let (tx2, mut rx2) = channel(1);
        let tx2 = Arc::new(tx2);

        let mut ds = ads.lock().await;
        ds.promote_to_active(upstairs_connection_1, tx1).await?;

        assert_eq!(ds.active_upstairs().len(), 1);
        assert!(matches!(rx1.try_recv().err().unwrap(), TryRecvError::Empty));
        assert!(matches!(rx2.try_recv().err().unwrap(), TryRecvError::Empty));

        ds.promote_to_active(upstairs_connection_2, tx2).await?;
        assert!(matches!(rx1.try_recv().err().unwrap(), TryRecvError::Empty));
        assert!(matches!(rx2.try_recv().err().unwrap(), TryRecvError::Empty));

        assert_eq!(ds.active_upstairs().len(), 2);

        assert!(ds.is_active(upstairs_connection_1));
        assert!(ds.is_active(upstairs_connection_2));

        Ok(())
    }

    #[tokio::test]
    async fn test_promote_to_active_multi_read_only_same_uuid() -> Result<()> {
        // Activating multiple read-only with the same Upstairs UUID should
        // kick out the other active one.
        let ads = build_test_downstairs(true)?;

        let upstairs_connection_1 = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 1,
        };

        let upstairs_connection_2 = UpstairsConnection {
            upstairs_id: upstairs_connection_1.upstairs_id,
            session_id: Uuid::new_v4(),
            gen: 1,
        };

        let (tx1, mut rx1) = channel(1);
        let tx1 = Arc::new(tx1);

        let (tx2, mut rx2) = channel(1);
        let tx2 = Arc::new(tx2);

        let mut ds = ads.lock().await;
        ds.promote_to_active(upstairs_connection_1, tx1).await?;

        assert_eq!(ds.active_upstairs().len(), 1);
        assert!(matches!(rx1.try_recv().err().unwrap(), TryRecvError::Empty));
        assert!(matches!(rx2.try_recv().err().unwrap(), TryRecvError::Empty));

        ds.promote_to_active(upstairs_connection_2, tx2).await?;
        assert_eq!(rx1.try_recv().unwrap(), upstairs_connection_2);
        assert!(matches!(rx2.try_recv().unwrap_err(), TryRecvError::Empty));

        assert_eq!(ds.active_upstairs().len(), 1);

        assert!(!ds.is_active(upstairs_connection_1));
        assert!(ds.is_active(upstairs_connection_2));

        Ok(())
    }

    #[tokio::test]
    async fn test_multiple_read_only_no_job_id_collision() -> Result<()> {
        // Two read-only Upstairs shouldn't see each other's jobs
        let ads = build_test_downstairs(true)?;

        let upstairs_connection_1 = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 1,
        };

        let upstairs_connection_2 = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 1,
        };

        let (tx1, mut rx1) = channel(1);
        let tx1 = Arc::new(tx1);

        let (tx2, mut rx2) = channel(1);
        let tx2 = Arc::new(tx2);

        let mut ds = ads.lock().await;
        ds.promote_to_active(upstairs_connection_1, tx1).await?;

        assert_eq!(ds.active_upstairs().len(), 1);
        assert!(matches!(rx1.try_recv().err().unwrap(), TryRecvError::Empty));
        assert!(matches!(rx2.try_recv().err().unwrap(), TryRecvError::Empty));

        ds.promote_to_active(upstairs_connection_2, tx2).await?;
        assert!(matches!(rx1.try_recv().err().unwrap(), TryRecvError::Empty));
        assert!(matches!(rx2.try_recv().unwrap_err(), TryRecvError::Empty));

        assert_eq!(ds.active_upstairs().len(), 2);

        let read_1 = IOop::Read {
            dependencies: Vec::new(),
            requests: vec![ReadRequest {
                eid: 0,
                offset: Block::new_512(1),
            }],
        };
        ds.add_work(upstairs_connection_1, 1000, read_1.clone())
            .await?;

        let read_2 = IOop::Read {
            dependencies: Vec::new(),
            requests: vec![ReadRequest {
                eid: 1,
                offset: Block::new_512(2),
            }],
        };
        ds.add_work(upstairs_connection_2, 1000, read_2.clone())
            .await?;

        let work_1 = ds.new_work(upstairs_connection_1).await?;
        let work_2 = ds.new_work(upstairs_connection_2).await?;

        assert_eq!(work_1, work_2);

        let job_1 = ds.get_job(upstairs_connection_1, 1000).await?;
        let job_2 = ds.get_job(upstairs_connection_2, 1000).await?;

        assert_eq!(job_1.upstairs_connection, upstairs_connection_1);
        assert_eq!(job_1.ds_id, 1000);
        assert_eq!(job_1.work, read_1);
        assert_eq!(job_1.state, WorkState::New);

        assert_eq!(job_2.upstairs_connection, upstairs_connection_2);
        assert_eq!(job_2.ds_id, 1000);
        assert_eq!(job_2.work, read_2);
        assert_eq!(job_2.state, WorkState::New);

        Ok(())
    }

    // Validate that `complete_work` cannot see None if the same Upstairs ID
    // (but a different session) goes active.
    #[tokio::test]
    async fn test_complete_work_cannot_see_none_same_upstairs_id() -> Result<()>
    {
        // Test region create and a read of one block.
        let block_size: u64 = 512;
        let extent_size = 4;

        // create region
        let mut region_options: crucible_common::RegionOptions =
            Default::default();
        region_options.set_block_size(block_size);
        region_options.set_extent_size(Block::new(
            extent_size,
            block_size.trailing_zeros(),
        ));
        region_options.set_uuid(Uuid::new_v4());

        let dir = tempdir()?;
        mkdir_for_file(dir.path())?;

        let mut region = Region::create(&dir, region_options, csl())?;
        region.extend(2)?;

        let path_dir = dir.as_ref().to_path_buf();
        let ads = build_downstairs_for_region(
            &path_dir,
            false,
            false,
            false,
            Some(csl()),
        )?;

        // This happens in proc() function.
        let upstairs_connection_1 = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 10,
        };

        // For the other_active_upstairs, unused.
        let (tx1, mut rx1) = channel(1);
        let tx1 = Arc::new(tx1);

        let mut ds = ads.lock().await;
        ds.promote_to_active(upstairs_connection_1, tx1).await?;

        // Add one job, id 1000
        let rio = IOop::Read {
            dependencies: Vec::new(),
            requests: vec![ReadRequest {
                eid: 0,
                offset: Block::new_512(1),
            }],
        };
        ds.add_work(upstairs_connection_1, 1000, rio).await?;

        // Now we mimic what happens in the do_work_task()
        let new_work = ds.new_work(upstairs_connection_1).await.unwrap();
        assert_eq!(new_work.len(), 1);

        let ip_id = ds.in_progress(upstairs_connection_1, 1000).await?.unwrap();
        assert_eq!(ip_id, 1000);
        let m = ds.do_work(upstairs_connection_1, 1000).await?.unwrap();

        // Before complete_work, say promote_to_active runs again for another
        // connection - same UUID, different session
        let upstairs_connection_2 = UpstairsConnection {
            upstairs_id: upstairs_connection_1.upstairs_id,
            session_id: Uuid::new_v4(),
            gen: 11,
        };

        let (_tx2, mut _rx2) = channel(1);
        let tx2 = Arc::new(_tx2);

        ds.promote_to_active(upstairs_connection_2, tx2).await?;

        assert_eq!(rx1.try_recv().unwrap(), upstairs_connection_2);

        // This should error with UpstairsInactive - upstairs_connection_1 isn't
        // active anymore and can't grab the work lock.
        let result = ds.complete_work(upstairs_connection_1, 1000, m).await;
        assert!(matches!(
            result.unwrap_err().downcast::<CrucibleError>().unwrap(),
            CrucibleError::UpstairsInactive,
        ));

        Ok(())
    }

    // Validate that `complete_work` cannot see None if a different Upstairs ID
    // goes active.
    #[tokio::test]
    async fn test_complete_work_cannot_see_none_different_upstairs_id(
    ) -> Result<()> {
        // Test region create and a read of one block.
        let block_size: u64 = 512;
        let extent_size = 4;

        // create region
        let mut region_options: crucible_common::RegionOptions =
            Default::default();
        region_options.set_block_size(block_size);
        region_options.set_extent_size(Block::new(
            extent_size,
            block_size.trailing_zeros(),
        ));
        region_options.set_uuid(Uuid::new_v4());

        let dir = tempdir()?;
        mkdir_for_file(dir.path())?;

        let mut region = Region::create(&dir, region_options, csl())?;
        region.extend(2)?;

        let path_dir = dir.as_ref().to_path_buf();
        let ads = build_downstairs_for_region(
            &path_dir,
            false,
            false,
            false,
            Some(csl()),
        )?;

        // This happens in proc() function.
        let upstairs_connection_1 = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 10,
        };

        // For the other_active_upstairs, unused.
        let (tx1, mut rx1) = channel(1);
        let tx1 = Arc::new(tx1);

        let mut ds = ads.lock().await;
        ds.promote_to_active(upstairs_connection_1, tx1).await?;

        // Add one job, id 1000
        let rio = IOop::Read {
            dependencies: Vec::new(),
            requests: vec![ReadRequest {
                eid: 0,
                offset: Block::new_512(1),
            }],
        };
        ds.add_work(upstairs_connection_1, 1000, rio).await?;

        // Now we mimic what happens in the do_work_task()
        let new_work = ds.new_work(upstairs_connection_1).await.unwrap();
        assert_eq!(new_work.len(), 1);

        let ip_id = ds.in_progress(upstairs_connection_1, 1000).await?.unwrap();
        assert_eq!(ip_id, 1000);
        let m = ds.do_work(upstairs_connection_1, 1000).await?.unwrap();

        // Before complete_work, say promote_to_active runs again for another
        // connection
        let upstairs_connection_2 = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 11,
        };

        let (_tx2, mut _rx2) = channel(1);
        let tx2 = Arc::new(_tx2);

        ds.promote_to_active(upstairs_connection_2, tx2).await?;

        assert_eq!(rx1.try_recv().unwrap(), upstairs_connection_2);

        // This should error with UpstairsInactive - upstairs_connection_1 isn't
        // active anymore and can't grab the work lock.
        let result = ds.complete_work(upstairs_connection_1, 1000, m).await;
        assert!(matches!(
            result.unwrap_err().downcast::<CrucibleError>().unwrap(),
            CrucibleError::UpstairsInactive,
        ));

        Ok(())
    }

    // Validate that `complete_work` can see None if the same Upstairs
    // reconnects.  We know it's the same Upstairs because the session and
    // upstairs ids will match.
    #[tokio::test]
    async fn test_complete_work_can_see_none() -> Result<()> {
        // Test region create and a read of one block.
        let block_size: u64 = 512;
        let extent_size = 4;

        // create region
        let mut region_options: crucible_common::RegionOptions =
            Default::default();
        region_options.set_block_size(block_size);
        region_options.set_extent_size(Block::new(
            extent_size,
            block_size.trailing_zeros(),
        ));
        region_options.set_uuid(Uuid::new_v4());

        let dir = tempdir()?;
        mkdir_for_file(dir.path())?;

        let mut region = Region::create(&dir, region_options, csl())?;
        region.extend(2)?;

        let path_dir = dir.as_ref().to_path_buf();
        let ads = build_downstairs_for_region(
            &path_dir,
            false,
            false,
            false,
            Some(csl()),
        )?;

        // This happens in proc() function.
        let upstairs_connection_1 = UpstairsConnection {
            upstairs_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            gen: 10,
        };

        // For the other_active_upstairs, unused.
        let (tx1, mut rx1) = channel(1);
        let tx1 = Arc::new(tx1);

        let mut ds = ads.lock().await;
        ds.promote_to_active(upstairs_connection_1, tx1).await?;

        // Add one job, id 1000
        let rio = IOop::Read {
            dependencies: Vec::new(),
            requests: vec![ReadRequest {
                eid: 0,
                offset: Block::new_512(1),
            }],
        };
        ds.add_work(upstairs_connection_1, 1000, rio).await?;

        // Now we mimic what happens in the do_work_task()
        let new_work = ds.new_work(upstairs_connection_1).await.unwrap();
        assert_eq!(new_work.len(), 1);

        let ip_id = ds.in_progress(upstairs_connection_1, 1000).await?.unwrap();
        assert_eq!(ip_id, 1000);
        let m = ds.do_work(upstairs_connection_1, 1000).await?.unwrap();

        // Before complete_work, the same Upstairs reconnects and goes active
        let (_tx2, mut _rx2) = channel(1);
        let tx2 = Arc::new(_tx2);

        ds.promote_to_active(upstairs_connection_1, tx2).await?;

        // In the real downstairs, there would be two tasks now that both
        // correspond to upstairs_connection_1.

        // Validate that the original set of tasks were sent the termination
        // signal.

        assert_eq!(rx1.try_recv().unwrap(), upstairs_connection_1);

        // If the original set of tasks don't end right away, they'll try to run
        // complete_work:

        let result = ds.complete_work(upstairs_connection_1, 1000, m).await;

        // `complete_work` will return Ok(()) despite not doing anything to the
        // Work struct.
        assert!(result.is_ok());

        Ok(())
    }
}
