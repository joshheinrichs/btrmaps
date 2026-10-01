mod atlas;
mod btrfs;
mod elevate;
mod gpu;
mod hilbert;
mod mounts;
mod palette;
mod proto;
mod scan;
mod stats;
mod tiles;
mod ui;
mod worker;

use anyhow::{Result, bail};

/// `btrmaps` opens the app. `btrmaps scan` is the root helper the app runs through sudo.
const USAGE: &str = "\
usage: btrmaps
       btrmaps scan PATH   (root; answers probe requests on stdin, what the app runs via sudo)";

fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.as_slice() {
        [] => ui::run(),
        [cmd, path] if cmd == "scan" => match scan::run(scan::Args { path: path.into() }) {
            // The app went away or moved on; that is how a scan gets stopped.
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe) =>
            {
                Ok(())
            }
            r => r,
        },
        _ => bail!(USAGE),
    }
}
