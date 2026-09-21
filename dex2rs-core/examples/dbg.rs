use dex2rs_core::dexinput::decode_method;
use dex2rs_core::ssa::IrBuilder;
use smali::types::SmaliClass;

const SRC: &str = r#"
.class public Lcom/example/Hello;
.super Ljava/lang/Object;

.method public static sum(I)I
    .registers 4

    const/4 v0, 0x0

    const/4 v1, 0x0

    :loop
    if-ge v1, p0, :end

    add-int/2addr v0, v1

    add-int/lit8 v1, v1, 0x1

    goto :loop

    :end
    return v0
.end method
"#;

fn main() {
    let class = SmaliClass::from_smali(SRC).unwrap();
    for m in &class.methods {
        let input = decode_method(&class, m, false, false).unwrap();
        println!("ops:");
        for (i, op) in input.ops.iter().enumerate() {
            println!("  {i}: {:?}  (mnemonic {})", op.op, op.mnemonic);
        }
        println!("labels: {:?}", input.labels);
        let b = IrBuilder::new(&input);
        match b.process() {
            Ok(im) => {
                println!("entry {:?} rpo {:?}", im.ir.entry, im.ir.rpo);
                for blk in 0..im.ir.blocks.len() {
                    let bl = &im.ir.blocks[blk];
                    println!(
                        "  block {} start={} end={} num={} ins={:?}",
                        blk,
                        bl.start,
                        bl.end,
                        bl.num,
                        bl.ins.len()
                    );
                }
                println!("offset_to_node {:?}", im.ir.offset_to_node);
                match im.get_source() {
                    Ok(s) => println!("{s}"),
                    Err(e) => println!("CODEGEN ERR {e}"),
                }
            }
            Err(e) => println!("ERR {e}"),
        }
    }
}
