//! Control flow graph — port of `dex2c/graph.py`.
//!
//! androguard built the basic blocks; here they are derived directly from the
//! linear instruction list (branch targets and try/catch boundaries become block
//! leaders), which is the same rule androguard used.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::dexinput::{ExceptionRange, MethodInput};
use crate::ir::{Block, BlockId, Ir, LandingPad, LocIdx};

/// Is this mnemonic one that ends a basic block?
///
/// androguard compiled `BRANCH_DVM_OPCODES` as *regular expressions* and matched
/// them against the instruction name with `re.match`, so `"if."` matched `if-eq`
/// (the `.` is any character). The equivalent prefix tests are spelled out here.
fn is_branch(mnemonic: &str) -> bool {
    mnemonic.starts_with("throw")
        || (mnemonic.starts_with("if") && mnemonic.len() > 2)
        || mnemonic.starts_with("goto")
        || mnemonic.starts_with("return")
        || mnemonic == "packed-switch"
        || mnemonic == "sparse-switch"
}

/// Whether this instruction can raise a Java exception, and therefore has an
/// edge to the handler of the `try` range that covers it.
///
/// This is the *safe set*: the ops that provably cannot throw. Anything
/// unrecognised is treated as throwing, so the list can only ever be widened
/// with care -- dropping an op that does throw costs the handler an operand,
/// while listing one that cannot just restores the old (block-end) behaviour.
/// The DEX verifier computes the handler's register types from the throwing
/// instructions alone, which is why the caller uses this to decide which
/// block-end states may reach a handler.
pub fn can_throw(mnemonic: &str) -> bool {
    if mnemonic.starts_with("move")      // move, move-result, move-exception, ...
        || mnemonic == "nop"
        || mnemonic.starts_with("const") // const-class/string resolve at load
        || mnemonic.starts_with("goto")
        || mnemonic.starts_with("if-")
        || mnemonic.starts_with("cmp")   // cmpl-/cmpg-/cmp-long
        || mnemonic.starts_with("return")
        || mnemonic == "packed-switch"
        || mnemonic == "sparse-switch"
        || mnemonic == "instance-of"
        // Conversions and bitwise negation.
        || mnemonic.starts_with("neg-")
        || mnemonic.starts_with("not-")
        || mnemonic.starts_with("int-to-")
        || mnemonic.starts_with("long-to-")
        || mnemonic.starts_with("float-to-")
        || mnemonic.starts_with("double-to-")
    {
        return false;
    }
    // Arithmetic/logic that cannot divide by zero. `div-` and `rem-` are the
    // integral ops that raise `ArithmeticException`, so they stay out; the
    // float forms are exempt in DEX but are listed as throwing unless the
    // mnemonic proves otherwise, which is the conservative direction.
    const SAFE_BINOPS: [&str; 9] = [
        "add-", "sub-", "mul-", "and-", "or-", "xor-", "shl-", "shr-", "ushr-",
    ];
    !SAFE_BINOPS.iter().any(|op| mnemonic.starts_with(op))
}

/// Instruction span of one basic block: `[start, end)` in instruction indices.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: LocIdx,
    pub end: LocIdx,
}

impl Span {
    fn contains(&self, idx: LocIdx) -> bool {
        self.start <= idx && idx < self.end
    }
}

/// Successors of a branch instruction, in androguard's `determineNext` order.
fn branch_successors(input: &MethodInput, idx: usize) -> Result<Vec<LocIdx>, String> {
    use smali::smali_ops::DexOp;
    let op = &input.ops[idx].op;
    let target = |label: &smali::smali_ops::Label| -> Result<LocIdx, String> {
        input
            .labels
            .get(label.0.strip_prefix(':').unwrap_or(&label.0))
            .copied()
            .ok_or_else(|| {
                format!(
                    "unknown branch target {} in {}->{}{}",
                    label, input.class_name, input.name, input.proto
                )
            })
    };
    Ok(match op {
        DexOp::Throw { .. } | DexOp::ThrowVerificationError { .. } => Vec::new(),
        DexOp::ReturnVoid
        | DexOp::Return { .. }
        | DexOp::ReturnWide { .. }
        | DexOp::ReturnObject { .. } => Vec::new(),
        DexOp::Goto { offset } | DexOp::Goto16 { offset } | DexOp::Goto32 { offset } => {
            vec![target(offset)?]
        }
        DexOp::IfEq { offset, .. }
        | DexOp::IfNe { offset, .. }
        | DexOp::IfLt { offset, .. }
        | DexOp::IfGe { offset, .. }
        | DexOp::IfGt { offset, .. }
        | DexOp::IfLe { offset, .. }
        | DexOp::IfEqz { offset, .. }
        | DexOp::IfNez { offset, .. }
        | DexOp::IfLtz { offset, .. }
        | DexOp::IfGez { offset, .. }
        | DexOp::IfGtz { offset, .. }
        | DexOp::IfLez { offset, .. } => {
            vec![(idx + 1) as LocIdx, target(offset)?]
        }
        DexOp::PackedSwitch { offset, .. } | DexOp::SparseSwitch { offset, .. } => {
            let label = offset.0.strip_prefix(':').unwrap_or(&offset.0);
            let mut succs = vec![(idx + 1) as LocIdx];
            match input.payloads.get(label) {
                Some(crate::dexinput::Payload::Packed { targets, .. }) => {
                    succs.extend(targets.iter().copied())
                }
                Some(crate::dexinput::Payload::Sparse(entries)) => {
                    succs.extend(entries.iter().map(|(_, t)| *t))
                }
                _ => {
                    return Err(format!(
                        "missing switch payload :{label} in {}->{}{}",
                        input.class_name, input.name, input.proto
                    ));
                }
            }
            succs
        }
        _ => Vec::new(),
    })
}

/// Split the linear instruction list into basic blocks (leader analysis).
pub fn basic_block_spans(input: &MethodInput) -> Result<Vec<Span>, String> {
    let n = input.ops.len();
    let mut leaders: BTreeSet<LocIdx> = BTreeSet::new();
    leaders.insert(0);

    for idx in 0..n {
        if !is_branch(&input.ops[idx].mnemonic) {
            continue;
        }
        for succ in branch_successors(input, idx)? {
            if succ >= 0 && succ < n as LocIdx {
                leaders.insert(succ);
            }
        }
    }
    for range in &input.exceptions {
        leaders.insert(range.start);
        // androguard did not cut blocks at the try end; doing so keeps a block
        // that starts inside a try from escaping the protected range.
        leaders.insert(range.end);
        for (_, handler) in &range.handlers {
            leaders.insert(*handler);
        }
    }

    let mut spans: Vec<Span> = Vec::new();
    let mut cur_start: LocIdx = 0;
    let mut cur_len = 0usize;
    for idx in 0..n {
        let idx = idx as LocIdx;
        if leaders.contains(&idx) && cur_len != 0 {
            spans.push(Span {
                start: cur_start,
                end: idx,
            });
            cur_start = idx;
            cur_len = 0;
        }
        cur_len += 1;
        if is_branch(&input.ops[idx as usize].mnemonic) {
            spans.push(Span {
                start: cur_start,
                end: idx + 1,
            });
            cur_start = idx + 1;
            cur_len = 0;
        }
    }
    if cur_len != 0 {
        spans.push(Span {
            start: cur_start,
            end: n as LocIdx,
        });
    }
    Ok(spans)
}

/// The successor spans of a block (branch targets / fallthrough).
fn span_successors(
    input: &MethodInput,
    spans: &[Span],
    span_idx: usize,
) -> Result<Vec<usize>, String> {
    let span = spans[span_idx];
    let last = (span.end - 1) as usize;
    let mnemonic = &input.ops[last].mnemonic;
    let find = |loc: LocIdx| spans.iter().position(|s| s.contains(loc));
    let mut out = Vec::new();
    if is_branch(mnemonic) {
        for succ in branch_successors(input, last)? {
            if let Some(i) = find(succ) {
                out.push(i);
            }
        }
    } else if let Some(i) = find(span.end) {
        // Fallthrough to the next block.
        out.push(i);
    }
    Ok(out)
}

/// `Exceptions.get_exception()`: the try range covering this block, if any.
fn exception_for(exceptions: &[ExceptionRange], span: Span) -> Option<usize> {
    exceptions
        .iter()
        .position(|r| r.start <= span.start && span.end <= r.end)
}

/// Build the CFG in `ir` from a decoded method (port of `graph.construct`).
///
/// Only blocks reachable from the entry block exist afterwards — matching the
/// Python implementation, which never materialised unreachable code.
pub fn construct(ir: &mut Ir, input: &MethodInput) -> Result<(), String> {
    let spans = basic_block_spans(input)?;
    if spans.is_empty() {
        return Err("method has no instructions".to_string());
    }
    let start_span = spans
        .iter()
        .position(|s| s.contains(0))
        .ok_or_else(|| "no block starts at instruction 0".to_string())?;

    // exception range index -> block span index of the first user (the pad's name)
    let mut exception_pad_source: BTreeMap<usize, BlockId> = BTreeMap::new();

    let mut span_to_block: BTreeMap<usize, BlockId> = BTreeMap::new();
    let mut visit_order: Vec<usize> = Vec::new();

    let mut queue: VecDeque<usize> = VecDeque::new();
    queue.push_back(start_span);

    while let Some(span_idx) = queue.pop_front() {
        if span_to_block.contains_key(&span_idx) {
            continue;
        }
        let span = spans[span_idx];
        let block_id = BlockId(ir.blocks.len() as u32);
        let mut block = Block::new(span.start);
        block.end = span.end;
        ir.blocks.push(block);
        span_to_block.insert(span_idx, block_id);
        visit_order.push(span_idx);

        // androguard's BFS enqueues exception handlers before normal children.
        if let Some(range_idx) = exception_for(&input.exceptions, span) {
            for (_, handler) in &input.exceptions[range_idx].handlers {
                if let Some(i) = spans.iter().position(|s| s.contains(*handler)) {
                    if !span_to_block.contains_key(&i) {
                        queue.push_back(i);
                    }
                }
            }
        }
        for succ in span_successors(input, &spans, span_idx)? {
            if !span_to_block.contains_key(&succ) {
                queue.push_back(succ);
            }
        }
    }

    ir.edges = vec![Vec::new(); ir.blocks.len()];
    ir.catch_edges = vec![Vec::new(); ir.blocks.len()];
    ir.reverse_edges = vec![Vec::new(); ir.blocks.len()];
    ir.reverse_catch_edges = vec![Vec::new(); ir.blocks.len()];

    // Second pass, in the order the blocks were discovered.
    for span_idx in visit_order.clone() {
        let node = span_to_block[&span_idx];
        let span = spans[span_idx];

        if let Some(range_idx) = exception_for(&input.exceptions, span) {
            // A pad is shared by every block with the same try range.
            let pad_idx = match exception_pad_source.get(&range_idx) {
                Some(src) => ir
                    .landing_pads
                    .iter()
                    .position(|p| p.source == *src)
                    .ok_or_else(|| "landing pad bookkeeping out of sync".to_string())?,
                None => {
                    exception_pad_source.insert(range_idx, node);
                    ir.landing_pads.push(LandingPad {
                        source: node,
                        handles: Vec::new(),
                    });
                    ir.landing_pads.len() - 1
                }
            };

            for (exception, handler) in &input.exceptions[range_idx].handlers {
                let Some(catch_span) = spans.iter().position(|s| s.contains(*handler)) else {
                    return Err(format!(
                        "handler offset {handler} is not a block start in {}->{}{}",
                        input.class_name, input.name, input.proto
                    ));
                };
                let catch_node = span_to_block[&catch_span];
                // Last handler wins, exactly like `set_catch_type` in the port source.
                ir.blocks[catch_node.0 as usize].catch_type = Some(exception.clone());
                ir.blocks[catch_node.0 as usize].in_catch = true;
                let pad = &mut ir.landing_pads[pad_idx];
                if !pad.handles.iter().any(|(t, _)| t == exception) {
                    pad.handles.push((exception.clone(), catch_node));
                }
            }

            for (_, handler) in &input.exceptions[range_idx].handlers {
                let catch_span = spans
                    .iter()
                    .position(|s| s.contains(*handler))
                    .ok_or_else(|| "handler block vanished".to_string())?;
                let catch_node = span_to_block[&catch_span];
                if !ir.blocks[node.0 as usize]
                    .catch_successors
                    .contains(&catch_node)
                {
                    ir.blocks[node.0 as usize].catch_successors.push(catch_node);
                }
                ir.add_catch_edge(node, catch_node);
            }
            ir.node_to_landing_pad.insert(node, pad_idx);
        }

        for succ in span_successors(input, &spans, span_idx)? {
            let child = span_to_block[&succ];
            ir.add_edge(node, child);
        }
    }

    ir.entry = Some(span_to_block[&start_span]);

    compute_rpo(ir);

    ir.offset_to_node.clear();
    for node in ir.rpo.clone() {
        let start = ir.blocks[node.0 as usize].start;
        if start >= 0 {
            ir.offset_to_node.insert(start, node);
        }
    }

    // A block whose predecessors are all catch handlers is itself "in catch".
    for node in ir.rpo.clone() {
        let preds: Vec<BlockId> = ir
            .all_preds(node)
            .into_iter()
            .filter(|p| ir.blocks[p.0 as usize].num < ir.blocks[node.0 as usize].num)
            .collect();
        if !preds.is_empty() && preds.iter().all(|p| ir.blocks[p.0 as usize].in_catch) {
            ir.blocks[node.0 as usize].in_catch = true;
        }
    }

    Ok(())
}

/// `Graph.compute_rpo()` — number blocks in reverse post order.
pub fn compute_rpo(ir: &mut Ir) {
    let nb = ir.blocks.len() as i64 + 1;
    let order = post_order(ir);
    for node in order {
        let po = ir.blocks[node.0 as usize].po;
        ir.blocks[node.0 as usize].num = nb - po;
    }
    let mut rpo: Vec<BlockId> = (0..ir.blocks.len() as u32).map(BlockId).collect();
    rpo.sort_by_key(|n| ir.blocks[n.0 as usize].num);
    ir.rpo = rpo;
}

/// `Graph.post_order()` — iterative DFS post-order from the entry block.
fn post_order(ir: &mut Ir) -> Vec<BlockId> {
    let Some(entry) = ir.entry else {
        return Vec::new();
    };
    let mut visited: BTreeSet<BlockId> = BTreeSet::new();
    let mut finished: Vec<BlockId> = Vec::new();
    let mut counter: i64 = 1;

    // (node, next successor index)
    let mut stack: Vec<(BlockId, usize)> = vec![(entry, 0)];
    visited.insert(entry);

    while let Some((node, idx)) = stack.pop() {
        let succs = ir.all_sucs(node);
        if idx < succs.len() {
            stack.push((node, idx + 1));
            let succ = succs[idx];
            if visited.insert(succ) {
                stack.push((succ, 0));
            }
        } else {
            ir.blocks[node.0 as usize].po = counter;
            counter += 1;
            finished.push(node);
        }
    }

    finished
}

/// `Graph.compute_block_order()` — order blocks by code position and renumber
/// them; this numbering is what the generated `LN` labels use.
pub fn compute_block_order(ir: &mut Ir) -> Vec<BlockId> {
    let mut order: Vec<BlockId> = (0..ir.blocks.len() as u32).map(BlockId).collect();
    order.sort_by_key(|n| ir.blocks[n.0 as usize].start);
    for (num, node) in order.iter().enumerate() {
        ir.blocks[node.0 as usize].num = num as i64;
    }
    order
}
