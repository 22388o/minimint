//! A library for cooperative port allocation between multiple processes.
//!
//! Fedimint tests in many places need to allocate ranges of unused ports for
//! Federations and other software under tests, without being able to `bind`
//! them beforehand.
//!
//! We used to mitigate that using a global per-process atomic counter, as
//! as simple port allocation mechanism. But this does not prevent conflicts
//! between different processes.
//!
//! Normally this would prevent us from running multiple tests at the same time,
//! which also makes it impossible to use `cargo nextest`.
//!
//! This library keeps track of allocated ports (with an expiration timeout) in
//! a shared file, protected by an advisory fs lock, and uses `bind` to make
//! sure a given port is actually free

pub mod data;
pub mod envs;
pub mod util;

use std::net::TcpListener;
use std::path::PathBuf;

// ports below 10k are typically used by normal software increasing change they
// would get in a way
pub const LOW: u16 = 10000;
// ports above 32k are typically ephmeral increasing a chance of random conflict
// after port was already tried
pub const HIGH: u16 = 32000;

use anyhow::bail;
use tracing::{debug, trace, warn};

use crate::data::DataDir;
use crate::envs::FM_PORTALLOC_DATA_DIR_ENV;

const LOG_PORT_ALLOC: &str = "port-alloc";

type UnixTimestamp = u64;

pub fn now_ts() -> UnixTimestamp {
    fedimint_core::time::duration_since_epoch().as_secs()
}

pub fn data_dir() -> anyhow::Result<PathBuf> {
    if let Some(env) = std::env::var_os(FM_PORTALLOC_DATA_DIR_ENV) {
        Ok(PathBuf::from(env))
    } else if let Some(dir) = dirs::cache_dir() {
        Ok(dir.join("fm-portalloc"))
    } else {
        bail!("Could not determine port alloc data dir. Try setting FM_PORTALLOC_DATA_DIR");
    }
}
pub fn port_alloc(range_size: u16) -> anyhow::Result<u16> {
    trace!(target: LOG_PORT_ALLOC, range_size, "Looking for port");
    if range_size == 0 {
        bail!("Can't allocate range of 0 bytes");
    }

    let mut data_dir = DataDir::new(data_dir()?)?;

    data_dir.with_lock(|data_dir| {
        let mut data = data_dir.load_data(now_ts())?;
        let mut base_port: u16 = data.next;
        Ok('retry: loop {
            trace!(target: LOG_PORT_ALLOC, base_port, range_size, "Checking a port");
            if HIGH < base_port {
                data = data.reclaim(now_ts());
                base_port = LOW;
            }
            let range = base_port..base_port + range_size;
            if let Some(next_port) = data.contains(range.clone()) {
                warn!(
                    base_port,
                    range_size,
                    "Could not use a port (already reserved). Will try a different range."
                );
                base_port = next_port;
                continue 'retry;
            }

            for port in range.clone() {
                match TcpListener::bind(("127.0.0.1", port)) {
                    Err(error) => {
                        warn!(
                            ?error,
                            port, "Could not use a port. Will try a different range"
                        );
                        base_port = port + 1;
                        continue 'retry;
                    }
                    Ok(l) => l,
                };
            }

            const ALLOCATION_TIME_SECS: u64 = 120;

            // The caller gets some time actually start using the port (`bind`),
            // to prevent other callers from re-using it. This could typically be
            // much shorter, as portalloc will not only respect the allocation,
            // but also try to bind before using a given port range. But for tests
            // that temporarily release ports (e.g. restarts, failure simulations, etc.),
            // there's a chance that this can expire and another tests snatches the test,
            // so better to keep it around the time a longest test can take.
            data.insert(range, now_ts() + ALLOCATION_TIME_SECS);

            data_dir.store_data(&data)?;

            debug!(target: LOG_PORT_ALLOC, base_port, range_size, "Allocated port range");
            break base_port;
        })
    })
}
