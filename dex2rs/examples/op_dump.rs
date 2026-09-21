//! Print the ops the front-end decoded for the methods a filter selects.
//!
//! ```text
//! cargo run -p dex2rs --example op_dump -- classes.dex filter.txt
//! cargo run -p dex2rs --example op_dump -- Fixture.smali out.dex
//! ```
//!
//! A `.smali` input is assembled first (and written to the second argument when
//! one is given), which is how a hand-written fixture — the only way to produce
//! an instruction that no compiler emits — can be run through the real
//! pipeline.
//!
//! This is the inspection tool for instructions the compiler rarely sees
//! (`invoke-polymorphic`, `invoke-custom`, `const-method-handle`, ...): it shows
//! the exact `DexOp` the `smali` crate produced, which is what `opcodes.rs`
//! matches on. Use it before writing a lowering for a new instruction.

use std::path::Path;

use smali::types::SmaliOp;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let input_path = args.next().unwrap_or_else(|| "classes.dex".to_string());
    let filter_path = args.next().unwrap_or_else(|| "filter.txt".to_string());
    // Optional substring filter on the method name, for a noisy dex.
    let needle = args.next();

    // A `.smali` input is assembled into a dex first; with a second path, that
    // dex is also written out (and can then be fed back in as the input).
    let input_path = if input_path.ends_with(".smali") {
        let text = std::fs::read_to_string(&input_path)?;
        let class = smali::types::SmaliClass::from_smali(&text)?;
        let dex = smali::dex::DexFile::from_smali(std::slice::from_ref(&class))?;
        let out = filter_path.ends_with(".dex").then(|| filter_path.clone());
        let path = match out {
            Some(path) => path,
            None => {
                let path = std::env::temp_dir().join("op_dump-assembled.dex");
                path.to_string_lossy().to_string()
            }
        };
        std::fs::write(&path, dex.to_bytes())?;
        eprintln!("assembled {} -> {path}", input_path);
        path
    } else {
        input_path
    };
    let filter_path = if filter_path.ends_with(".dex") {
        // No filter was given; select everything.
        let path = std::env::temp_dir().join("op_dump-all.filter");
        std::fs::write(&path, ".*;.*\n")?;
        path.to_string_lossy().to_string()
    } else {
        filter_path
    };

    let input = dex2rs::apk::load(Path::new(&input_path))?;
    let classes: Vec<smali::types::SmaliClass> = input
        .dexes()
        .iter()
        .flat_map(|unit| unit.classes.iter().cloned())
        .collect();
    let filter =
        dex2rs::filter::MethodFilter::new(Path::new(&filter_path), &classes, false, false)?;

    for class in &classes {
        for method in &class.methods {
            if !filter.should_compile(class, method) {
                continue;
            }
            if let Some(needle) = &needle
                && !method.name.contains(needle.as_str())
            {
                continue;
            }
            let ops: Vec<&SmaliOp> = method
                .ops
                .iter()
                .filter(|op| matches!(op, SmaliOp::Op(_)))
                .collect();
            println!(
                "\n=== {}->{}{:?}  ({} instructions)",
                class.name,
                method.name,
                method.signature,
                ops.len()
            );
            for op in ops {
                if let SmaliOp::Op(op) = op {
                    println!("    {op:?}");
                }
            }
        }
    }
    Ok(())
}
