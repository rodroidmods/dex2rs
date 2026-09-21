//! Dump a single method's lowered form straight from a `.smali` file.
//!
//! `cfg_dump` needs an APK; this takes the compiler's own test fixture format,
//! which is what a minimal reproduction of a CFG/SSA bug is written in:
//!
//! ```text
//! cargo run -p dex2rs --example smali_graph -- dex2rs-core/tests/fixtures/Hello.smali reuseAfterThrow
//! ```

use dex2rs_core::ssa::IrBuilder;
use smali::types::SmaliClass;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args.first().cloned().unwrap_or_default();
    let want = args.get(1).cloned().unwrap_or_default();

    let text = std::fs::read_to_string(&path)?;
    let class = SmaliClass::from_smali(&text)?;

    for method in &class.methods {
        if method.name != want {
            continue;
        }
        let mut input = dex2rs_core::dexinput::decode_method(&class, method, false, false)?;
        input.trace = false;
        println!(
            "{}->{}{}",
            class.name.as_jni_type(),
            method.name,
            method.signature.to_jni()
        );
        println!("exceptions: {:?}", input.exceptions);
        for (i, op) in input.ops.iter().enumerate() {
            println!("  [{i:3}] {:<18} {}", op.mnemonic, op.operands);
        }
        println!();

        let mut builder = IrBuilder::new(&input);
        dex2rs_core::graph::construct(&mut builder.ir, &input)?;
        // Build first: the catch-edge state a block hands to its handler is
        // recorded while the block is lowered, so it is only visible after.
        let built = builder.build();
        for (idx, block) in builder.ir.blocks.iter().enumerate() {
            let fmt = |v: &Vec<dex2rs_core::BlockId>| -> String {
                format!("{:?}", v.iter().map(|b| b.0).collect::<Vec<u32>>())
            };
            println!(
                "block {idx} [{}..{}){} succ={} catch_succ={} catch_values={:?}",
                block.start,
                block.end,
                if block.in_catch { " IN-CATCH" } else { "" },
                fmt(builder.ir.edges.get(idx).unwrap()),
                fmt(builder.ir.catch_edges.get(idx).unwrap()),
                block
                    .catch_values
                    .as_ref()
                    .map(|m| m.keys().collect::<Vec<_>>()),
            );
        }
        println!();
        for (i, value) in builder.ir.values.iter().enumerate() {
            let id = dex2rs_core::ValId(i as u32);
            let def = value
                .definition
                .map(|d| builder.ir.ins_summary(d))
                .unwrap_or_else(|| "-".into());
            println!(
                "  #{i:<4} {:<14} {:<16} def={:<20}",
                builder.ir.value_name(id),
                value.describe(),
                def
            );
            if let dex2rs_core::ir::ValKind::Phi { operands, .. } = &value.kind {
                for (block, operand) in operands {
                    println!("         <- {block:?}: {}", builder.ir.value_name(*operand));
                }
            }
        }
        println!();
        match built {
            Ok(()) => println!("ssa: ok"),
            Err(e) => println!("ssa: FAILED: {e}"),
        }
        return Ok(());
    }
    eprintln!("method not found");
    Ok(())
}
