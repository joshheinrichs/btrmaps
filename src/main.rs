mod btrfs;
mod proto;
mod scan;
mod treemap;
mod view;

use anyhow::{Context, Result, bail};
use std::path::PathBuf;

/// `btrmaps` opens the app. `btrmaps scan` is the root helper the app runs through sudo.
const USAGE: &str = "\
usage: btrmaps
       btrmaps scan [--order N] [-o SCAN.jsonl] PATH   (root; what the app runs via sudo)";

fn parse_scan(argv: &[String]) -> Result<scan::Args> {
    let (mut path, mut out, mut order) = (None, None, 10);
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-o" | "--output" => out = Some(PathBuf::from(it.next().context("-o needs a file")?)),
            "--order" => {
                order = it.next().context("--order needs a number")?.parse()?;
                if !(1..=12).contains(&order) {
                    bail!("--order must be between 1 and 12");
                }
            }
            _ if path.is_none() => path = Some(PathBuf::from(arg)),
            _ => bail!("unexpected argument {arg:?}\n\n{USAGE}"),
        }
    }
    Ok(scan::Args {
        path: path.context(USAGE)?,
        out,
        order,
    })
}

fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.split_first() {
        None => view::run(),
        Some((cmd, rest)) if cmd == "scan" => match scan::run(parse_scan(rest)?) {
            // The app went away or moved on; that is how a scan gets stopped.
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe) =>
            {
                Ok(())
            }
            r => r,
        },
        Some(_) => bail!(USAGE),
    }
}
