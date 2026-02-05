mod bench;
mod bench_rules;
mod bench_runners;
mod bench_types;

use std::env::{args, current_dir};
use std::io;
use std::time::Duration;

use tokio::runtime::Builder;

fn main() -> io::Result<()> {
    let runtime = Builder::new_multi_thread().enable_all().build()?;
    let outcome = runtime.block_on(async_main());
    runtime.shutdown_timeout(Duration::from_millis(0));
    outcome
}

async fn async_main() -> io::Result<()> {
    bench::run_and_print(&current_dir()?, &(args().skip(1).collect::<Vec<_>>())).await
}
