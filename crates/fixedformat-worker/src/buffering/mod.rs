//! Table-buffering functions exposed by the fixedformat worker.

mod write_fixed;

use vgi::Worker;

/// Register every buffering function on the worker.
pub fn register(worker: &mut Worker) {
    worker.register_buffering(write_fixed::WriteFixed);
}
