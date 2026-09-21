//! SSA construction — port of the `IrBuilder` / `RegisterAllocator` / `IrMethod`
//! classes in `dex2c/compiler.py`.
//!
//! The algorithm is the one described in Braun et al., "Simple and Efficient
//! Construction of Static Single Assignment Form": blocks are filled one by one
//! and sealed once all their predecessors are known, which lets phis be created
//! lazily (`read_variable_recursive`) without a separate dominance frontier pass.
//!
//! ## Deviation from the Python original
//!
//! The C++ writer had no support for phi nodes at all: `RegisterAllocator` was
//! seeded from `entry.var_to_declare`, which only ever received *instructions'*
//! values, so a live phi reaching `Writer.ra()` raised `KeyError`, the exception
//! bubbled into `DvClass.process` and the whole method was skipped. Here live
//! phis are declared as well and the code generator lowers them by copying the
//! incoming operand at the end of each predecessor (see `codegen`). Methods that
//! the Python tool could not compile therefore still compile, and every method
//! the Python tool *did* compile produces the same shape of code.

use std::collections::VecDeque;

use crate::block::fill_node_from_block;
use crate::dexinput::MethodInput;
use crate::graph;
use crate::ir::{BlockId, InsId, InsKind, Ir, User, ValId, ValKind};
use crate::util;

/// `RegisterAllocator`: one slot per `(register, host type)` pair.
#[derive(Debug, Default)]
pub struct RegisterAllocator {
    slots: std::collections::BTreeMap<(i64, String), usize>,
    next_name: usize,
}

impl RegisterAllocator {
    pub fn build(ir: &Ir, vars: &[ValId]) -> Result<Self, String> {
        let mut ra = RegisterAllocator::default();
        for var in vars {
            ra.allocate(ir, *var)?;
        }
        Ok(ra)
    }

    /// `RegisterAllocator.allocate`
    pub fn allocate(&mut self, ir: &Ir, var: ValId) -> Result<usize, String> {
        let register = ir.value(var).register();
        let atype = ir
            .value(var)
            .get_type()
            .ok_or_else(|| format!("unknown type for {}", ir.value_name(var)))?;
        let key = (register, util::get_cdecl_type(atype).to_string());
        if let Some(slot) = self.slots.get(&key) {
            return Ok(*slot);
        }
        let slot = self.next_name;
        self.slots.insert(key, slot);
        self.next_name += 1;
        Ok(slot)
    }

    /// `self.ra(var)` in the writer.
    pub fn get(&self, ir: &Ir, var: ValId) -> Result<usize, String> {
        let register = ir.value(var).register();
        let atype = ir.value(var).get_type().unwrap_or("");
        let key = (register, util::get_cdecl_type(atype).to_string());
        self.slots
            .get(&key)
            .copied()
            .ok_or_else(|| format!("no register allocated for {}", ir.value_name(var)))
    }
}

/// Everything the code generator needs about one method.
#[derive(Debug)]
pub struct IrMethod {
    pub ir: Ir,
    pub ra: RegisterAllocator,
    /// Blocks in code order (the order `L%d` labels are emitted in).
    pub irblocks: Vec<BlockId>,
    pub rtype: String,
    pub params: Vec<i64>,
    pub params_type: Vec<String>,
    pub dynamic_register: bool,
}

impl IrMethod {
    pub fn new(mut ir: Ir, dynamic_register: bool) -> Result<Self, String> {
        let irblocks = ir.compute_block_order();
        let entry = ir
            .entry
            .ok_or_else(|| "method has no entry block".to_string())?;
        let vars = ir.block(entry).var_to_declare.clone();
        let ra = RegisterAllocator::build(&ir, &vars)?;
        Ok(IrMethod {
            rtype: ir.rtype.clone(),
            params: ir.params.clone(),
            params_type: ir.params_type.clone(),
            ir,
            ra,
            irblocks,
            dynamic_register,
        })
    }

    /// `IrMethod.get_source()`
    pub fn get_source(&self) -> Result<String, String> {
        crate::codegen::Writer::new(self)?.write_method()
    }

    /// `IrMethod.get_prototype()` (used when registering natives dynamically).
    pub fn get_prototype(&self) -> Result<String, String> {
        crate::codegen::Writer::new(self)?.prototype()
    }
}

/// `IrBuilder` from compiler.py.
pub struct IrBuilder<'a> {
    pub input: &'a MethodInput,
    pub ir: Ir,
    pub current_block: Option<BlockId>,
    /// Registers holding the incoming arguments (`lparams` in Python).
    pub lparams: Vec<i64>,
    pub params_type: Vec<String>,
    pub rtype: String,
    pub access: Vec<String>,
    pub obfus: bool,
    pub dynamic_register: bool,
    /// Emit per-instruction traces into the generated source.
    pub trace: bool,
}

impl<'a> IrBuilder<'a> {
    pub fn new(input: &'a MethodInput) -> Self {
        let params_type = input.params_type();
        let lparams = input.param_registers(&params_type);
        let rtype = input.return_type();
        let access = util::get_access_method(input.access);
        IrBuilder {
            input,
            ir: Ir::default(),
            current_block: None,
            lparams,
            params_type,
            rtype,
            access,
            obfus: input.obfus,
            dynamic_register: input.dynamic_register,
            trace: input.trace,
        }
    }

    pub fn get_return_type(&self) -> &str {
        &self.rtype
    }

    /// `IrBuilder.process()` — build the CFG + SSA and hand back the method.
    pub fn process(mut self) -> Result<IrMethod, String> {
        let input = self.input;
        self.ir.class_name = input.class_name.clone();
        self.ir.name = input.name.clone();
        self.ir.proto = input.proto.clone();
        self.ir.rtype = self.rtype.clone();
        self.ir.params = self.lparams.clone();
        self.ir.params_type = self.params_type.clone();
        self.ir.access = self.access.clone();
        self.ir.is_static = input.is_static;
        self.ir.dynamic_register = self.dynamic_register;
        self.ir.obfus = self.obfus;
        self.ir.trace = self.trace;
        self.ir.registers_size = input.registers_size as i64;
        self.ir.ins_size = input.ins_size as i64;

        graph::construct(&mut self.ir, input)?;
        self.build()?;

        let dynamic_register = self.dynamic_register;
        IrMethod::new(self.ir, dynamic_register)
    }

    /// `IrBuilder.build()`
    pub fn build(&mut self) -> Result<(), String> {
        self.define_params()?;

        let entry = self
            .ir
            .entry
            .ok_or_else(|| "method has no entry block".to_string())?;
        self.ir.block_mut(entry).sealed = true;

        let mut todo: VecDeque<BlockId> = VecDeque::new();
        todo.push_back(entry);
        while let Some(node) = todo.pop_front() {
            if self.ir.block(node).filled {
                continue;
            }
            self.current_block = Some(node);
            fill_node_from_block(self, node)?;
            self.try_seal_block(node)?;
            for succ in self.ir.all_sucs(node) {
                todo.push_back(succ);
            }
        }

        self.remove_trivial_phi();
        self.hack_polymorphic_constant()?;
        self.infer_type()?;
        self.fix_const_type()?;
        self.verify_operand_type()?;
        self.verify_phi_operand_type()?;
        self.add_var_to_decl();

        Ok(())
    }

    /// `IrBuilder.remove_trivial_phi()`
    pub fn remove_trivial_phi(&mut self) {
        let mut changed = true;
        while changed {
            changed = false;
            for node in self.ir.rpo.clone() {
                let phis = self.ir.block(node).phis.clone();
                for phi in phis {
                    changed |= self.ir.phi_remove_trivial_phi(phi);
                }
            }
        }
    }

    /// The instruction that writes `value`, if any writes it at all.
    ///
    /// Only for the diagnostic below: `Value::definition` is not maintained for
    /// every kind of writer, and when a type goes missing the producing
    /// instruction is the first thing anyone needs.
    fn producer_of(&self, value: ValId) -> Option<&InsKind> {
        for node in self.ir.rpo.clone() {
            for ins in &self.ir.block(node).ins {
                if self.ir.ins(*ins).value() == Some(value) {
                    return Some(&self.ir.ins(*ins).kind);
                }
            }
        }
        None
    }

    /// `IrBuilder.verify_operand_type()`
    ///
    /// A value that never received a type means some instruction left its result
    /// uninferred, and the emitter cannot pick a register width for it. The
    /// instruction that *defines* the value is what identifies the bug, so name
    /// it and its operands rather than only the value: the reference's
    /// "unknown type v13_9" says which value, but not which opcode produced it,
    /// which is the first thing anyone needs to reproduce it.
    pub fn verify_operand_type(&mut self) -> Result<(), String> {
        for node in self.ir.compute_block_order() {
            for ins in self.ir.block(node).ins.clone() {
                if let Some(var) = self.ir.ins(ins).value() {
                    if self.ir.value(var).get_type().is_none() {
                        let mut detail = format!(
                            "unknown type {} (defined by {:?})",
                            self.ir.value_name(var),
                            self.ir.ins(ins).kind
                        );
                        // An array element's type comes from the array's type, so
                        // when it fails the useful question is "what was the array
                        // then?" -- and, one step further back, what produced the
                        // array. That chain is what identifies the missing rule.
                        let operands: &[ValId] = match &self.ir.ins(ins).kind {
                            InsKind::ArrayLoad { array, .. } => std::slice::from_ref(array),
                            InsKind::ArrayStore { array, .. } => std::slice::from_ref(array),
                            _ => &[],
                        };
                        for operand in operands {
                            detail.push_str(&format!(
                                "; array {} has type {:?}",
                                self.ir.value_name(*operand),
                                self.ir.value(*operand).get_type()
                            ));
                            if let Some(definition) = self.ir.value(*operand).definition {
                                detail.push_str(&format!(
                                    ", defined by {:?}",
                                    self.ir.ins(definition).kind
                                ));
                            } else if let Some(producer) = self.producer_of(*operand) {
                                // Not every writer records itself in
                                // `Value::definition`, and the producer is the
                                // only thing that says *why* the type is what it
                                // is, so look it up.
                                detail.push_str(&format!(", produced by {producer:?}"));
                            }
                            // Where the type came from decides which rule to look
                            // at: a phi merges its operands' types, a `Var` at
                            // version 0 is a parameter (its type is the method
                            // descriptor's), and anything else was set by the
                            // defining instruction above.
                            match &self.ir.value(*operand).kind {
                                ValKind::Phi { operands, .. } => {
                                    let types: Vec<String> = operands
                                        .values()
                                        .map(|v| {
                                            self.ir.value(*v).get_type().unwrap_or("?").to_string()
                                        })
                                        .collect();
                                    detail.push_str(&format!(
                                        "; it is a phi over [{}]",
                                        types.join(", ")
                                    ));
                                }
                                ValKind::Var { register, version } => detail.push_str(&format!(
                                    "; it is a variable r{register} v{version}"
                                )),
                                ValKind::Const { .. } => detail.push_str("; it is a constant"),
                            }
                        }
                        return Err(detail);
                    }
                }
            }
        }
        Ok(())
    }

    /// `IrBuilder.verify_phi_operand_type()`
    pub fn verify_phi_operand_type(&mut self) -> Result<(), String> {
        for node in self.ir.rpo.clone() {
            for phi in self.ir.block(node).phis.clone() {
                let same_type = self.ir.value(phi).get_type().map(|s| s.to_string());
                let operands: Vec<ValId> = match self.ir.phi_operands(phi) {
                    Some(map) => map.values().copied().collect(),
                    None => continue,
                };
                for operand in operands {
                    let op_type = self.ir.value(operand).get_type().map(|s| s.to_string());
                    if !types_compatible(same_type.as_deref(), op_type.as_deref()) {
                        return Err(format!(
                            "inconsistency phi operand type {} {} {}",
                            self.ir.value_name(phi),
                            same_type.as_deref().unwrap_or("?"),
                            op_type.as_deref().unwrap_or("?")
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// `IrBuilder.hack_polymorphic_constant()`
    ///
    /// A constant such as `0` can be used as `null`, as `false` and as the int
    /// zero; when the type inference cannot decide, every user gets its own copy
    /// of the constant so each can be typed independently.
    pub fn hack_polymorphic_constant(&mut self) -> Result<(), String> {
        let mut todo: Vec<InsId> = Vec::new();
        for node in self.ir.compute_block_order() {
            for ins in self.ir.block(node).ins.clone() {
                if let InsKind::LoadConstant { value, .. } = &self.ir.ins(ins).kind {
                    if self.ir.value(*value).get_type().is_none() {
                        todo.push(ins);
                    }
                }
            }
        }

        while !todo.is_empty() {
            let ins = todo.remove(0);
            let (old_val, cst, obfus, parent) = match &self.ir.ins(ins).kind {
                InsKind::LoadConstant { value, cst, obfus } => {
                    (*value, *cst, *obfus, self.ir.ins(ins).parent)
                }
                _ => continue,
            };
            let users = self.ir.users_of_ins(ins);
            for user in users {
                let register = self.ir.value(old_val).register();
                let new_val = self.write_variable(register);
                let new_ins = self.ir.add_ins(InsKind::LoadConstant {
                    value: new_val,
                    cst,
                    obfus,
                });
                self.ir.value_mut(new_val).is_const = true;
                if let Some(bb) = parent {
                    if let Some(pos) = self.ir.block(bb).ins.iter().position(|i| *i == ins) {
                        self.ir.block_mut(bb).ins.insert(pos, new_ins);
                    }
                }
                match user {
                    User::Ins(u) => self.ir.replace_use_of_with(u, old_val, new_val),
                    User::Phi(p) => self.ir.phi_replace_use_of_with(p, old_val, new_val),
                }
            }
            if let Some(bb) = parent {
                self.ir.block_mut(bb).ins.retain(|i| *i != ins);
            }
        }
        Ok(())
    }

    /// `IrBuilder.infer_type()`
    pub fn infer_type(&mut self) -> Result<(), String> {
        let mut changed = true;
        let nodes = self.ir.compute_block_order();
        let mut max = 500;
        while changed {
            max -= 1;
            if max == 0 {
                return Err("type infer failed".to_string());
            }
            changed = false;
            for node in &nodes {
                let phis = self.ir.block(*node).phis.clone();
                for phi in phis {
                    changed |= self.ir.phi_resolve_type(phi)?;
                }
                for ins in self.ir.block(*node).ins.clone() {
                    changed |= self
                        .ir
                        .resolve_type(ins)
                        .map_err(|e| format!("{e} (resolving {})", self.ir.ins_summary(ins)))?;
                }
            }
        }
        Ok(())
    }

    /// `IrBuilder.fix_const_type()` — constants the inference left untyped get the
    /// narrowest primitive type that can hold them.
    pub fn fix_const_type(&mut self) -> Result<(), String> {
        let mut changed = false;
        for node in self.ir.rpo.clone() {
            for ins in self.ir.block(node).ins.clone() {
                let is_untyped_load = match &self.ir.ins(ins).kind {
                    InsKind::LoadConstant { value, .. } => {
                        self.ir.value(*value).get_type().is_none()
                    }
                    _ => false,
                };
                if !is_untyped_load {
                    continue;
                }
                let (value, cst) = match &self.ir.ins(ins).kind {
                    InsKind::LoadConstant { value, cst, .. } => (*value, *cst),
                    _ => continue,
                };
                let constant = self.ir.constant_int(cst).unwrap_or(0);
                let ty = if (-2147483648..=2147483647).contains(&constant) {
                    "I"
                } else {
                    "J"
                };
                changed = true;
                self.ir.value_mut(value).set_type(Some(ty));
            }
        }
        if changed {
            self.infer_type()?;
        }
        Ok(())
    }

    /// `IrBuilder.add_var_to_decl()`
    pub fn add_var_to_decl(&mut self) {
        let nodes = self.ir.compute_block_order();
        let mut vars: Vec<ValId> = Vec::new();
        let mut classes: Vec<String> = Vec::new();
        let mut fields: Vec<String> = Vec::new();
        let mut methods: Vec<String> = Vec::new();

        for node in &nodes {
            for ins in self.ir.block(*node).ins.clone() {
                if let Some(var) = self.ir.ins(ins).value() {
                    vars.push(var);
                }
                if let Some(clz) = self.ir.ins_get_class(ins) {
                    classes.push(clz);
                }
                if let Some(field) = self.ir.ins_get_field(ins) {
                    fields.push(field);
                }
                if let Some(method) = self.ir.ins_get_call_method(ins) {
                    methods.push(method);
                }
            }
        }

        // See the module comment: the Python writer crashed on live phis, here
        // they are declared so the method can still be compiled.
        for node in &nodes {
            for phi in self.ir.block(*node).phis.clone() {
                if self.ir.use_count(phi) > 0 {
                    vars.push(phi);
                }
            }
        }

        let Some(entry) = self.ir.entry else {
            return;
        };
        let block = self.ir.block_mut(entry);
        block.var_to_declare.extend(vars);
        block.class_to_declare.extend(classes);
        block.field_to_declare.extend(fields);
        block.method_to_declare.extend(methods);
    }

    /// `IrBuilder.try_seal_block()`
    pub fn try_seal_block(&mut self, block: BlockId) -> Result<(), String> {
        let sucs = self.ir.all_sucs(block);
        if sucs.is_empty() {
            return Ok(());
        }
        for suc in sucs {
            let preds = self.ir.all_preds(suc);
            let filled = preds
                .iter()
                .filter(|pred| self.ir.block(**pred).filled)
                .count();
            if filled == preds.len() {
                self.seal_block(suc)?;
            }
        }
        Ok(())
    }

    /// `IrBuilder.seal_block()`
    pub fn seal_block(&mut self, block: BlockId) -> Result<(), String> {
        let preds = self.ir.all_preds(block);
        self.ir.block_mut(block).sealed = true;
        let incomplete = self.ir.block(block).incomplete_phis.clone();
        for phi in incomplete {
            for pred in &preds {
                let register = self.ir.value(phi).register();
                let incoming = self.incoming_from(register, *pred, block)?;
                self.ir.phi_add_operand(phi, *pred, incoming);
            }
            self.ir.block_mut(block).phis.push(phi);
        }
        self.ir.block_mut(block).incomplete_phis.clear();
        Ok(())
    }

    /// The value a phi in `block` should take from `pred`.
    ///
    /// A normal edge hands over the predecessor's end state. A *catch* edge
    /// does not: control leaves `pred` at the instruction that threw, so the
    /// value has to come from `pred.catch_values` (recorded at that
    /// instruction) rather than from anything the block computed afterwards.
    /// Falling back to the normal read keeps registers the snapshot does not
    /// mention -- ones defined before the block -- resolving as before.
    fn incoming_from(
        &mut self,
        register: i64,
        pred: BlockId,
        block: BlockId,
    ) -> Result<ValId, String> {
        if self.ir.catch_edges[pred.0 as usize].contains(&block) {
            if let Some(value) = self.ir.block(pred).catch_values.as_ref() {
                if let Some(value) = value.get(&register) {
                    return Ok(*value);
                }
            }
            // Not live at the throw point, yet written somewhere in the block:
            // that write is *after* the throw, so it can never reach the
            // handler. Hiding it makes the read fall through to the value the
            // block came in with, which is what the register actually held when
            // the exception was raised.
            //
            // Only attempted when the predecessor has a single predecessor of
            // its own, so the read resolves through it with no phi. With more
            // than one, resolving "the block's entry value" means manufacturing
            // a fresh phi mid-seal, and its operands would be read from this
            // same end state -- the wrong value, one level down.
            let single_pred = self.ir.all_preds(pred).len() == 1
                && self
                    .ir
                    .block(pred)
                    .current_definitions
                    .contains_key(&register);
            if single_pred {
                let saved = self
                    .ir
                    .block_mut(pred)
                    .current_definitions
                    .remove(&register);
                let incoming = self.read_variable_in(register, pred);
                if let Some(value) = saved {
                    self.ir
                        .block_mut(pred)
                        .current_definitions
                        .insert(register, value);
                }
                return incoming;
            }
        }
        self.read_variable_in(register, pred)
    }

    pub fn set_current_block(&mut self, block: BlockId) {
        self.current_block = Some(block);
    }

    /// `IrBuilder.read_result_variable()`
    pub fn read_result_variable(&mut self) -> Result<ValId, String> {
        let var = self.read_variable(-1)?;
        if self.ir.value(var).is_phi() {
            return Err("invoke result is a Phi node".to_string());
        }
        Ok(var)
    }

    /// `IrBuilder.write_result_variable()`
    pub fn write_result_variable(&mut self) -> ValId {
        self.write_variable(-1)
    }

    /// `IrBuilder.write_variable()`
    pub fn write_variable(&mut self, register: i64) -> ValId {
        let value = self.new_ssa_variable(register, false);
        if let Some(block) = self.current_block {
            self.ir
                .block_mut(block)
                .current_definitions
                .insert(register, value);
        }
        value
    }

    /// `IrBuilder.read_variable()`
    pub fn read_variable(&mut self, register: i64) -> Result<ValId, String> {
        let block = self
            .current_block
            .ok_or_else(|| "read_variable without a current block".to_string())?;
        self.read_variable_in(register, block)
    }

    /// `IrBuilder.read_variable(register, block)`
    pub fn read_variable_in(&mut self, register: i64, block: BlockId) -> Result<ValId, String> {
        if let Some(value) = self.ir.block(block).current_definitions.get(&register) {
            return Ok(*value);
        }
        self.read_variable_recursive(register, block)
    }

    /// `IrBuilder.read_variable_recursive()`
    pub fn read_variable_recursive(
        &mut self,
        register: i64,
        block: BlockId,
    ) -> Result<ValId, String> {
        let preds = self.ir.all_preds(block);
        let value;
        if !self.ir.block(block).sealed {
            value = self.new_ssa_variable(register, true);
            self.ir.block_mut(block).incomplete_phis.push(value);
            self.ir.set_phi_block(value, block);
        } else if preds.len() == 1 {
            // A handler is often sealed as soon as its predecessor is filled, so
            // this shortcut runs before `seal_block` ever sees the phi. It has to
            // apply the same catch-edge rule, or a register the predecessor
            // rewrote after its last throwing instruction leaks in.
            value = self.incoming_from(register, preds[0], block)?;
        } else {
            value = self.new_ssa_variable(register, true);
            self.ir
                .block_mut(block)
                .current_definitions
                .insert(register, value);
            self.ir.block_mut(block).phis.push(value);
            self.ir.set_phi_block(value, block);
            for pred in preds {
                let incoming = self.incoming_from(register, pred, block)?;
                self.ir.phi_add_operand(value, pred, incoming);
            }
        }
        self.ir
            .block_mut(block)
            .current_definitions
            .insert(register, value);
        Ok(value)
    }

    /// `IrBuilder.define_params()`
    pub fn define_params(&mut self) -> Result<(), String> {
        let entry = self
            .ir
            .entry
            .ok_or_else(|| "method has no entry block".to_string())?;
        let input = self.input;

        let mut register = input.registers_size as i64 - input.ins_size as i64;
        if !input.is_static {
            let param = self.new_ssa_variable(register, false);
            let cls = self.ir.class_name.clone();
            self.ir.value_mut(param).set_type(Some(&cls));
            let ins = self.ir.add_ins(InsKind::MoveParam {
                param: crate::ir::Param {
                    value: param,
                    this: true,
                    declared: true,
                },
            });
            let entry_block = self.ir.block_mut(entry);
            entry_block.move_params.push(ins);
            entry_block.current_definitions.insert(register, param);
            entry_block.var_to_declare.push(param);
            register += 1;
        }

        let params_type = self.params_type.clone();
        for ptype in &params_type {
            let param = self.new_ssa_variable(register, false);
            self.ir.value_mut(param).set_type(Some(ptype));
            let ins = self.ir.add_ins(InsKind::MoveParam {
                param: crate::ir::Param {
                    value: param,
                    this: false,
                    declared: true,
                },
            });
            let entry_block = self.ir.block_mut(entry);
            entry_block.move_params.push(ins);
            entry_block.current_definitions.insert(register, param);
            entry_block.var_to_declare.push(param);
            register += util::get_type_size(ptype);
        }
        Ok(())
    }

    /// `IrBuilder.new_ssa_variable()`
    fn new_ssa_variable(&mut self, register: i64, phi: bool) -> ValId {
        let version = *self.ir.var_versions.get(&register).unwrap_or(&0);
        self.ir.var_versions.insert(register, version + 1);
        if phi {
            self.ir.new_phi(register, version)
        } else {
            self.ir.new_var(register, version)
        }
    }
}

/// `is_int`/`is_float`/`is_ref` compatibility test used by the phi verifier.
fn types_compatible(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            (util::is_int(a) && util::is_int(b))
                || (util::is_float(a) && util::is_float(b))
                || (util::is_ref(a) && util::is_ref(b))
        }
        _ => false,
    }
}
