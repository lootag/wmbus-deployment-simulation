//! DSMR5 against DSMR6 gas meters on wireless M-Bus, in a dense district.
//!
//! ```text
//! cargo run --release                                         # every variant, dashboards in dashboards/
//! cargo run --release -- --variants dsmr5,dsmr6-by-the-book   # a selection
//! cargo run --release -- --config configs/worst-case-6000.json --out dashboards/worst-case-6000
//! cargo run --release -- --print-config                       # every parameter, as JSON
//! ```

mod district;
mod frames;
mod gateway;
mod meter;
mod params;
mod radio;
mod sim;

use std::error::Error;
use std::path::Path;

use params::Params;

fn main() -> Result<(), Box<dyn Error>> {
    let mut p = Params::default();
    let mut out = String::from("dashboards");
    let mut only: Option<Vec<String>> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                let path = args.next().ok_or("--config needs a file")?;
                p = serde_json::from_str(&std::fs::read_to_string(path)?)?;
            }
            "--out" => out = args.next().ok_or("--out needs a directory")?,
            "--variants" => {
                let list = args.next().ok_or("--variants needs a comma-separated list")?;
                only = Some(list.split(',').map(|v| v.trim().to_string()).collect());
            }
            "--print-config" => {
                println!("{}", serde_json::to_string_pretty(&p)?);
                return Ok(());
            }
            "-h" | "--help" => {
                println!("usage: wmbus-deployment-simulation [--config FILE] [--variants a,b] [--out DIR] [--print-config]");
                return Ok(());
            }
            other => return Err(format!("unknown argument {other}").into()),
        }
    }

    let d = p.district.clone();
    let all = district::variants();
    if let Some(only) = &only {
        for name in only {
            if !all.iter().any(|v| v.file == name) {
                let known: Vec<_> = all.iter().map(|v| v.file).collect();
                return Err(format!("unknown variant {name}; known: {}", known.join(", ")).into());
            }
        }
    }
    let mut rows = Vec::new();
    for v in all.into_iter().filter(|v| only.as_ref().is_none_or(|o| o.iter().any(|n| n == v.file))) {
        let t = std::time::Instant::now();
        let (steady, cut) = district::run_variant(&p, &d, &v, Path::new(&out))?;
        eprintln!("  {} done in {:.0} s", v.name, t.elapsed().as_secs_f64());
        rows.push((v, steady, cut));
    }
    district::print(&d, &rows);
    Ok(())
}
