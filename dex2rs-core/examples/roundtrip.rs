//! Round-trip a dex through the `smali` crate and report the size delta.
//!
//! `cargo run -p dex2rs-core --example roundtrip -- path/to/classes.dex`

use smali::dex::DexFile;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .expect("usage: roundtrip <classes.dex>");
    let bytes = std::fs::read(&path)?;
    let before = bytes.len();

    let dex = DexFile::from_bytes(&bytes)?;
    let classes = dex.to_smali()?;
    let lines: usize = classes
        .iter()
        .flat_map(|c| c.methods.iter())
        .flat_map(|m| m.ops.iter())
        .filter(|op| matches!(op, smali::types::SmaliOp::Line(_)))
        .count();
    let methods: usize = classes.iter().map(|c| c.methods.len()).sum();

    let rebuilt = DexFile::from_smali(&classes)?;
    let out = rebuilt.to_bytes().to_vec();

    println!("input         : {before:>10} bytes");
    println!(
        "output        : {:>10} bytes ({:+.1}%)",
        out.len(),
        (out.len() as f64 / before as f64 - 1.0) * 100.0
    );
    println!("classes       : {}", classes.len());
    println!("methods       : {methods}");
    println!("Line ops      : {lines}");

    // Which methods contribute the Line ops, and what does one look like?
    let mut per_class: Vec<(usize, String)> = classes
        .iter()
        .map(|c| {
            (
                c.methods
                    .iter()
                    .flat_map(|m| m.ops.iter())
                    .filter(|op| matches!(op, smali::types::SmaliOp::Line(_)))
                    .count(),
                c.name.as_java_type(),
            )
        })
        .collect();
    per_class.sort_unstable_by_key(|(n, _)| std::cmp::Reverse(*n));
    println!("top classes by Line ops:");
    for (n, name) in per_class.iter().take(5) {
        println!("  {n:>9}  {name}");
    }

    let sample = classes.iter().flat_map(|c| c.methods.iter()).find(|m| {
        m.ops
            .iter()
            .filter(|op| matches!(op, smali::types::SmaliOp::Line(_)))
            .count()
            > 5
    });
    if let Some(m) = sample {
        let mut hist: std::collections::BTreeMap<&str, usize> = Default::default();
        for op in &m.ops {
            let key = match op {
                smali::types::SmaliOp::Line(_) => "Line",
                smali::types::SmaliOp::Label(_) => "Label",
                smali::types::SmaliOp::Op(_) => "Op",
                _ => "other",
            };
            *hist.entry(key).or_default() += 1;
        }
        println!(
            "sample method {}{:?}, ops={}: {hist:?}",
            m.name,
            m.signature,
            m.ops.len()
        );
        println!("  first 30 ops:");
        for op in m.ops.iter().take(30) {
            match op {
                smali::types::SmaliOp::Line(l) => println!("    line {l}"),
                smali::types::SmaliOp::Op(o) => println!("    op {o:?}"),
                other => println!("    {other:?}"),
            }
        }
    }
    Ok(())
}
