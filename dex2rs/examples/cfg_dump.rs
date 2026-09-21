//! Dump a single method's lowered form: the basic-block graph, the exception
//! edges that feed each handler, and (with `--values`) every SSA value with the
//! type inference settled on, where that type came from, and who reads it.
//!
//! ```text
//! cargo run -p dex2rs --example cfg_dump -- app-release.apk \
//!     Lcom/android/apksig/internal/util/FileChannelDataSource; copyTo
//! cargo run -p dex2rs --example cfg_dump -- app-release.apk Lr2/n0; q0 --values
//! ```
//!
//! Written to answer questions the `unknown type` / `unable to refine type`
//! errors cannot: those name the value, and a wrong *but inferred* type (which
//! shows up as a Rust type error in the generated code instead) needs the whole
//! value graph -- who defined it, what it was sealed to and when, and who reads
//! it. Seeing that is what turns "the generated code does not compile" into a
//! rule that can be fixed.

use std::path::Path;

/// Print every block with its successors, the exception edges feeding it, and
/// (with `values`) the full lowered form of each instruction.
fn print_blocks(ir: &dex2rs_core::ir::Ir, values: bool) {
    let fmt = |v: &Vec<dex2rs_core::BlockId>| -> String {
        format!("{:?}", v.iter().map(|b| b.0).collect::<Vec<u32>>())
    };
    for (idx, block) in ir.blocks.iter().enumerate() {
        let ops: Vec<String> = if values {
            block
                .ins
                .iter()
                .map(|i| format!("{:?}", ir.ins(*i).kind))
                .collect()
        } else {
            block.ins.iter().map(|i| ir.ins_summary(*i)).collect()
        };
        println!(
            "block {idx} [{}..{}){} succ={} catch_succ={}",
            block.start,
            block.end,
            if block.in_catch { " IN-CATCH" } else { "" },
            fmt(ir.edges.get(idx).unwrap()),
            fmt(ir.catch_edges.get(idx).unwrap()),
        );
        println!(
            "    preds={} catch_preds={} catch_type={:?}",
            fmt(ir.reverse_edges.get(idx).unwrap()),
            fmt(ir.reverse_catch_edges.get(idx).unwrap()),
            block.catch_type
        );
        println!(
            "    phis: {:?}",
            block
                .phis
                .iter()
                .map(|p| ir.value_name(*p))
                .collect::<Vec<_>>()
        );
        println!("    ops: {}", ops.join("\n         "));
    }
}

/// Every SSA value with the type inference settled on, its sealed flags, its
/// definition and its readers -- the state a `unable to refine type` / `unknown
/// type` error has to be read against.
fn dump_values(
    ir: &dex2rs_core::ir::Ir,
    ra: Option<&dex2rs_core::ssa::RegisterAllocator>,
    verbose: bool,
) {
    println!("\nvalues (name, type, sealed flags, definition, readers):");
    for (i, value) in ir.values.iter().enumerate() {
        let id = dex2rs_core::ValId(i as u32);
        // `slot` is the emitted variable name (`v36`), which is how a line of
        // generated code maps back to this value.
        let slot = ra
            .and_then(|ra| ra.get(ir, id).ok())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "-".into());
        let def = value
            .definition
            .map(|d| ir.ins_summary(d))
            .unwrap_or_else(|| "-".into());
        let users: Vec<String> = value
            .uses
            .iter()
            .map(|u| match u {
                dex2rs_core::ir::User::Ins(i) => ir.ins_summary(*i),
                dex2rs_core::ir::User::Phi(_) => "phi".to_string(),
            })
            .collect();
        // For a phi, which incoming value each predecessor supplies is what a
        // mismatched copy in the generated code is usually about.
        let incoming = match &value.kind {
            dex2rs_core::ir::ValKind::Phi { operands, .. } if verbose => {
                let mut parts: Vec<String> = Vec::new();
                for (block, operand) in operands {
                    let incoming_slot = ra
                        .and_then(|ra| ra.get(ir, *operand).ok())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "-".into());
                    parts.push(format!(
                        "{block:?}:{}[{incoming_slot}]",
                        ir.value_name(*operand)
                    ));
                }
                format!("  <- {{{}}}", parts.join(", "))
            }
            _ => String::new(),
        };
        println!(
            "  #{i:<4} v{slot:<4} {:<14} {:<18} sealed={:<5} cast_sealed={:<5} def={:<16} users=[{}]{incoming}",
            ir.value_name(id),
            value.describe(),
            value.type_sealed,
            value.cast_sealed,
            def,
            users.join(", "),
        );
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let values = argv.iter().any(|a| a == "--values");
    let raw = argv.iter().any(|a| a == "--raw");
    // `--graph` stops after CFG construction: it still prints when SSA fails, so
    // a `unable to refine type` / `unknown type` error can be read against the
    // blocks and catch edges that produced it.
    let graph_only = argv.iter().any(|a| a == "--graph");
    let args: Vec<String> = argv
        .into_iter()
        .filter(|a| a != "--values" && a != "--raw" && a != "--graph")
        .collect();

    let apk = args
        .first()
        .cloned()
        .unwrap_or_else(|| "app-release.apk".into());
    let want_class = args.get(1).cloned().unwrap_or_default();
    let want_name = args.get(2).cloned().unwrap_or_default();
    let want_desc = args.get(3).cloned().unwrap_or_default();

    let input = dex2rs::apk::load(Path::new(&apk))?;
    let classes: Vec<smali::types::SmaliClass> = input
        .dexes()
        .iter()
        .flat_map(|unit| unit.classes.iter().cloned())
        .collect();

    for class in &classes {
        if class.name.as_jni_type() != want_class {
            continue;
        }
        for method in &class.methods {
            if method.name != want_name {
                continue;
            }
            let proto = method.signature.to_jni();
            if !want_desc.is_empty() && proto != want_desc {
                continue;
            }
            let mut method_input =
                dex2rs_core::dexinput::decode_method(class, method, false, false)?;
            method_input.trace = false;

            // `--raw` stops after decoding: the instruction stream and the
            // exception ranges, which is what an inference failure has to be
            // read against (the type rules run after).
            if raw {
                println!(
                    "{}->{}{}   ins={} outs={} regs={} locals={}",
                    class.name.as_jni_type(),
                    method.name,
                    proto,
                    method_input.ins_size,
                    method_input.registers_size - method_input.ins_size,
                    method_input.registers_size,
                    method_input.locals
                );
                for (i, op) in method_input.ops.iter().enumerate() {
                    println!("  [{i:4}] {op:?}");
                }
                println!("exceptions: {:?}", method_input.exceptions);
                return Ok(());
            }

            if graph_only {
                let mut builder = dex2rs_core::ssa::IrBuilder::new(&method_input);
                dex2rs_core::graph::construct(&mut builder.ir, &method_input)?;
                println!("exception ranges: {:?}", method_input.exceptions);
                // `build()` borrows the builder mutably and may fail part way
                // through, but the IR it filled in is still there -- that is
                // exactly the state a type error has to be read against, so the
                // dump comes after it rather than skipping on the error.
                let built = builder.build();
                print_blocks(&builder.ir, false);
                dump_values(&builder.ir, None, true);
                match built {
                    Ok(()) => println!("\nssa: ok"),
                    Err(e) => println!("\nssa: FAILED: {e}"),
                }
                return Ok(());
            }

            // Full pipeline, so the types printed are the ones the emitter sees.
            let ir_method = dex2rs_core::ssa::IrBuilder::new(&method_input).process()?;
            let ir = &ir_method.ir;

            println!(
                "{}->{}{}   blocks: {}   values: {}",
                class.name.as_jni_type(),
                method.name,
                proto,
                ir.blocks.len(),
                ir.values.len()
            );
            println!();
            println!("exception ranges (protected range -> handler block):");
            for (i, range) in method_input.exceptions.iter().enumerate() {
                println!(
                    "  #{} instrs [{}..{})  handlers {:?}",
                    i, range.start, range.end, range.handlers
                );
            }
            println!();
            print_blocks(ir, values);

            if values {
                // The emitted variable names are allocator slot numbers, so the
                // slot -> (register, host type) map is what links a line of
                // generated code back to a register.
                println!("\nallocator slots: {:?}", ir_method.ra);
                dump_values(ir, Some(&ir_method.ra), true);
            }
            return Ok(());
        }
    }
    eprintln!("method not found");
    Ok(())
}
