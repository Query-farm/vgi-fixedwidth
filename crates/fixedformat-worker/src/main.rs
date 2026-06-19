//! The `fixedformat` VGI worker.
//!
//! A standalone binary DuckDB launches and talks to over Apache Arrow IPC. It
//! brings Perl-`unpack` / Python-`struct` / COBOL-copybook fixed-width parsing
//! and formatting to SQL under the catalog `fixed`, schema `main`:
//!
//! - `fixed.main.unpack(rec, spec)` — parse a string/blob into a STRUCT
//! - `fixed.main.pack(struct, spec)` — format a STRUCT back into a BLOB
//! - `fixed.main.read_fixed(path, spec, ...)` — scan a fixed-width file
//! - `fixed.main.write_fixed((FROM rel), path, spec, ...)` — write one out

mod arrow_map;
mod buffering;
mod options;
mod scalar;
mod table;
mod value_in;

use vgi::Worker;

fn main() {
    // Logs MUST go to stderr — stdout is the Arrow-IPC channel.
    let _ = env_logger::Builder::from_env(env_logger::Env::default().filter_or("VGI_LOG", "info"))
        .format_timestamp_millis()
        .try_init();

    // The catalog name DuckDB sees in `ATTACH 'fixed' (TYPE vgi, …)`.
    if std::env::var_os("VGI_WORKER_CATALOG_NAME").is_none() {
        std::env::set_var("VGI_WORKER_CATALOG_NAME", "fixed");
    }

    let mut worker = Worker::new();
    scalar::register(&mut worker);
    table::register(&mut worker);
    buffering::register(&mut worker);
    worker.run();
}
