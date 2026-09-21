//! `fill_node_from_block` from `dex2c/basic_blocks.py`: turn one block's decoded
//! instructions into IR instructions linked into the arena.

use crate::ir::{BlockId, DvmInfo, InsId, InsKind, Ir};
use crate::ssa::IrBuilder;

/// Build the IR for one basic block. The block keeps its instruction order.
pub fn fill_node_from_block(b: &mut IrBuilder, node: BlockId) -> Result<(), String> {
    let (start, end) = {
        let block = b.ir.block(node);
        (block.start, block.end)
    };

    // Only a block covered by a `try` range can hand control to a handler, and
    // only at an instruction that can throw. The handler's phi reads the
    // registers *there*, so snapshot them before the instruction runs (a
    // throwing `invoke` never writes its result).
    let catch_protected = b.ir.node_to_landing_pad.contains_key(&node);

    let mut idx = start;
    while idx < end {
        let decoded = b.input.ops[idx as usize].clone();
        if catch_protected && crate::graph::can_throw(&decoded.mnemonic) {
            let snapshot = b.ir.block(node).current_definitions.clone();
            b.ir.block_mut(node).catch_values = Some(snapshot);
        }
        let built = crate::opcodes::build(b, idx as usize, node, &decoded.op)?;
        let Some(ins_id) = built else {
            idx += 1;
            continue;
        };

        // `aget-object` / `iget-object`: route the result through a temp so a
        // local reference living in the same register is not killed early.
        // (See the comment in fill_node_from_block.)
        let mut move_result: Option<InsId> = None;
        if matches!(
            decoded.op,
            smali::smali_ops::DexOp::AGetObject { .. } | smali::smali_ops::DexOp::IGetObject { .. }
        ) {
            let original =
                b.ir.ins(ins_id)
                    .value()
                    .ok_or_else(|| "aget/iget without a result register".to_string())?;
            let tmp = b.write_result_variable();
            b.ir.ins_mut(ins_id).kind.set_value(tmp);
            let move_ins = b.ir.add_ins(InsKind::MoveResult {
                lhs: original,
                rhs: tmp,
            });
            b.ir.ins_mut(move_ins).parent = Some(node);
            move_result = Some(move_ins);
        }

        {
            let dvm = if decoded.mnemonic.is_empty() {
                None
            } else {
                Some(DvmInfo {
                    name: decoded.mnemonic.clone(),
                    output: decoded.operands.clone(),
                })
            };
            let ins_node = b.ir.ins_mut(ins_id);
            ins_node.parent = Some(node);
            ins_node.offset = idx;
            ins_node.next_offset = idx + 1;
            ins_node.dvm = dvm;
        }
        if let Some(value) = b.ir.ins(ins_id).value() {
            b.ir.value_mut(value).definition = Some(ins_id);
        }

        // Push in the order the Python writer saw: the instruction, then the
        // synthesised move-result.
        let block = b.ir.block_mut(node);
        block.ins.push(ins_id);
        if let Some(mv) = move_result {
            block.ins.push(mv);
        }

        idx += 1;
    }

    b.ir.block_mut(node).filled = true;
    Ok(())
}

/// Helper used by the builder when a block already has an IR instruction list.
pub fn block_ins<'a>(ir: &'a Ir, node: BlockId) -> &'a [InsId] {
    &ir.block(node).ins
}
