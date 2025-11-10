use std::env;
use std::error::Error;

use sidebundle_closure::ClosureBuilder;
use sidebundle_core::{BundleEntry, BundleSpec, TargetTriple};
use sidebundle_packager::Packager;

fn main() {
    if let Err(err) = run() {
        eprintln!("sidebundle: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        println!("usage: sidebundle-cli <binary-path> ...");
        return Ok(());
    }

    let spec = args
        .iter()
        .enumerate()
        .fold(BundleSpec::new("demo", TargetTriple::linux_x86_64()), |bundle, (idx, path)| {
            let display = format!("entry-{idx}");
            bundle.with_entry(BundleEntry::new(path, display))
        });

    let closure = ClosureBuilder::new().build(&spec);
    Packager::new().emit(&spec, &closure)?;
    println!(
        "planned bundle `{}` with {} file(s)",
        spec.name,
        closure.files.len()
    );
    Ok(())
}
