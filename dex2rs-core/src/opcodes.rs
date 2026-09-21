//! `DexOp` -> IR bridge — port of `dex2c/opcode_ins.py`.
//!
//! One arm per dex opcode, in the same order as the Python instruction set, so
//! the two implementations can be diffed side by side. The SSA reads/writes are
//! performed in exactly the order the Python helpers did them: that order decides
//! which version numbers the variables get, and therefore the generated code.

use smali::smali_ops::{DexOp, Label, MethodRef, RegisterRange, SmaliRegister};

use crate::dexinput::Payload;
use crate::ir::{
    BinOp, CondOp, ConstInvokeValue, ConstVal, InsId, InsKind, InvokeData, InvokeType, LocIdx,
    PolymorphicInvokeData, PolymorphicKind, UnOp, ValId,
};
use crate::ssa::IrBuilder;
use crate::util;

fn reg(b: &IrBuilder, r: &SmaliRegister) -> i64 {
    crate::dexinput::resolve_register(r, b.input.registers_size, b.input.ins_size)
}

fn range_regs(b: &IrBuilder, r: &RegisterRange) -> Vec<i64> {
    let start = reg(b, &r.start);
    let end = reg(b, &r.end);
    if end < start {
        return Vec::new();
    }
    (start..=end).collect()
}

fn label_index(b: &IrBuilder, l: &Label) -> Result<LocIdx, String> {
    let name = l.0.strip_prefix(':').unwrap_or(&l.0);
    b.input.labels.get(name).copied().ok_or_else(|| {
        format!(
            "unknown branch target {} in {}->{}{}",
            l, b.input.class_name, b.input.name, b.input.proto
        )
    })
}

fn return_type_of(descriptor: &str) -> String {
    match descriptor.find(')') {
        Some(i) => descriptor[i + 1..].to_string(),
        None => "V".to_string(),
    }
}

// ------------------------------------------------------------- constants ----

fn int_const(b: &mut IrBuilder, value: i64, ty: Option<&str>) -> Result<ValId, String> {
    b.ir.new_const(ConstVal::Int(value), ty)
}

fn str_const(b: &mut IrBuilder, value: String, ty: Option<&str>) -> Result<ValId, String> {
    b.ir.new_const(ConstVal::Str(value), ty)
}

/// `LoadConstant(value, cst, obfus)`.
fn load_constant(
    b: &mut IrBuilder,
    value: ValId,
    cst: ValId,
    obfus: bool,
) -> Result<InsId, String> {
    let cst_type = b.ir.value(cst).get_type().map(|s| s.to_string());
    b.ir.value_mut(value).refine_type(cst_type.as_deref())?;
    b.ir.value_mut(value).is_const = true;
    Ok(b.ir.add_ins(InsKind::LoadConstant { value, cst, obfus }))
}

// -------------------------------------------------------------- helpers -----

fn get_variables(b: &mut IrBuilder, registers: &[i64]) -> Result<Vec<ValId>, String> {
    let mut out = Vec::with_capacity(registers.len());
    for r in registers {
        out.push(b.read_variable(*r)?);
    }
    Ok(out)
}

fn array_load(
    b: &mut IrBuilder,
    dest: i64,
    array: i64,
    index: i64,
    ar_type: Option<&str>,
) -> Result<InsId, String> {
    let array_val = b.read_variable(array)?;
    let index_val = b.read_variable(index)?;
    let value = b.write_variable(dest);
    Ok(b.ir.add_ins(InsKind::ArrayLoad {
        value,
        array: array_val,
        index: index_val,
        elem_type: ar_type.map(|s| s.to_string()),
    }))
}

fn array_store(
    b: &mut IrBuilder,
    src: i64,
    array: i64,
    index: i64,
    ar_type: Option<&str>,
) -> Result<InsId, String> {
    let rhs = b.read_variable(src)?;
    let array_val = b.read_variable(array)?;
    let index_val = b.read_variable(index)?;
    Ok(b.ir.add_ins(InsKind::ArrayStore {
        rhs,
        array: array_val,
        index: index_val,
        elem_type: ar_type.map(|s| s.to_string()),
    }))
}

fn binary_comp(
    b: &mut IrBuilder,
    op: BinOp,
    dest: i64,
    src1: i64,
    src2: i64,
    cmp_type: &str,
) -> Result<InsId, String> {
    let arg1 = b.read_variable(src1)?;
    let arg2 = b.read_variable(src2)?;
    let value = b.write_variable(dest);
    Ok(b.ir.add_ins(InsKind::Binary {
        op,
        value,
        arg1,
        arg2,
        op_type: cmp_type.to_string(),
        comp: true,
    }))
}

fn assign_binary(
    b: &mut IrBuilder,
    op: BinOp,
    op_type: &str,
    dest: i64,
    src1: i64,
    src2: i64,
) -> Result<InsId, String> {
    let arg2 = b.read_variable(src2)?;
    let arg1 = b.read_variable(src1)?;
    let value = b.write_variable(dest);
    b.ir.value_mut(value).refine_type(Some(op_type))?;
    Ok(b.ir.add_ins(InsKind::Binary {
        op,
        value,
        arg1,
        arg2,
        op_type: op_type.to_string(),
        comp: false,
    }))
}

fn assign_binary_2addr(
    b: &mut IrBuilder,
    op: BinOp,
    op_type: &str,
    dest: i64,
    src: i64,
) -> Result<InsId, String> {
    let arg1 = b.read_variable(dest)?;
    let arg2 = b.read_variable(src)?;
    let value = b.write_variable(dest);
    b.ir.value_mut(value).refine_type(Some(op_type))?;
    Ok(b.ir.add_ins(InsKind::Binary {
        op,
        value,
        arg1,
        arg2,
        op_type: op_type.to_string(),
        comp: false,
    }))
}

/// `assign_lit()` — `dest = src <op> #literal`.
fn assign_lit(
    b: &mut IrBuilder,
    op: BinOp,
    literal: i64,
    dest: i64,
    src: i64,
) -> Result<InsId, String> {
    let arg1 = b.read_variable(src)?;
    let value = b.write_variable(dest);
    b.ir.value_mut(value).refine_type(Some("I"))?;
    let cst = int_const(b, literal, Some("I"))?;
    Ok(b.ir.add_ins(InsKind::Binary {
        op,
        value,
        arg1,
        arg2: cst,
        op_type: "I".to_string(),
        comp: false,
    }))
}

fn unary(b: &mut IrBuilder, dest: i64, op: UnOp, src: i64, ty: &str) -> Result<InsId, String> {
    let arg = b.read_variable(src)?;
    let value = b.write_variable(dest);
    Ok(b.ir.add_ins(InsKind::Unary {
        value,
        op,
        arg,
        ty: ty.to_string(),
    }))
}

fn cast(
    b: &mut IrBuilder,
    dest: i64,
    op: &str,
    dest_type: &str,
    src: i64,
    src_type: &str,
) -> Result<InsId, String> {
    let arg = b.read_variable(src)?;
    b.ir.value_mut(arg).refine_type(Some(src_type))?;
    let value = b.write_variable(dest);
    b.ir.value_mut(value).refine_type(Some(dest_type))?;
    Ok(b.ir.add_ins(InsKind::Cast {
        value,
        op: op.to_string(),
        dest_type: dest_type.to_string(),
        arg,
        src_type: src_type.to_string(),
    }))
}

fn cond(
    b: &mut IrBuilder,
    op: CondOp,
    src1: i64,
    src2: i64,
    target: LocIdx,
) -> Result<InsId, String> {
    let arg1 = b.read_variable(src1)?;
    let arg2 = b.read_variable(src2)?;
    Ok(b.ir.add_ins(InsKind::Cond {
        op,
        arg1,
        arg2,
        target,
    }))
}

fn condz(b: &mut IrBuilder, op: CondOp, src: i64, target: LocIdx) -> Result<InsId, String> {
    let arg = b.read_variable(src)?;
    Ok(b.ir.add_ins(InsKind::CondZ { op, arg, target }))
}

fn get_field(field: &smali::smali_ops::FieldRef) -> (String, String, String) {
    (
        field.class.clone(),
        field.descriptor.clone(),
        field.name.clone(),
    )
}

fn get_args(b: &mut IrBuilder, param_type: &[String], largs: &[i64]) -> Result<Vec<ValId>, String> {
    let mut args = Vec::new();
    if param_type.len() > largs.len() {
        // `len(param_type) > len(largs)` — the Python helper logged a warning and
        // produced no arguments at all.
        return Ok(args);
    }
    let mut num_param = 0usize;
    for ptype in param_type {
        if num_param >= largs.len() {
            return Err("invoke register list is shorter than its prototype".to_string());
        }
        let value = b.read_variable(largs[num_param])?;
        args.push(value);
        num_param += util::get_type_size(ptype) as usize;
    }
    Ok(args)
}

fn invoke_common(
    b: &mut IrBuilder,
    invoke_type: InvokeType,
    method: &MethodRef,
    thiz_reg: Option<i64>,
    arg_regs: &[i64],
) -> Result<InsId, String> {
    let clsdesc = method.class.clone();
    let name = method.name.clone();
    let param_type = util::get_params_type(&method.descriptor);
    let rtype = return_type_of(&method.descriptor);

    let thiz = match thiz_reg {
        Some(r) => Some(b.read_variable(r)?),
        None => None,
    };
    let args = get_args(b, &param_type, arg_regs)?;
    let result = if rtype == "V" {
        None
    } else {
        Some(b.write_result_variable())
    };
    if let Some(result) = result {
        b.ir.value_mut(result).refine_type(Some(&rtype))?;
    }
    let triple = (clsdesc.clone(), name.clone(), method.descriptor.clone());
    let data = InvokeData {
        invoke_type,
        clsdesc,
        name,
        thiz,
        rtype,
        ptype: param_type,
        args,
        result,
        triple,
    };
    let _ = invoke_type.is_static();
    Ok(b.ir.add_ins(InsKind::Invoke(data)))
}

/// `invoke-polymorphic` / `invoke-polymorphic/range`.
///
/// Register 0 is the polymorphic receiver; the remaining registers hold the
/// call-site arguments, laid out per the *call-site prototype* (`proto`), which
/// is what the dex carries in place of a resolved method: it excludes the
/// receiver and gives the arguments' real types (`([III)I` is an `int[]`, an
/// `int` and an `int` returning `int`). The erased JNI descriptor on `method`
/// (`([Ljava/lang/Object;)Ljava/lang/Object;`) is only there to name the
/// polymorphic method, so the prototype is the type source here.
fn polymorphic_invoke(
    b: &mut IrBuilder,
    method: &MethodRef,
    proto: &str,
    regs: &[i64],
) -> Result<InsId, String> {
    let Some((&receiver_reg, arg_regs)) = regs.split_first() else {
        return Err(format!(
            "invoke-polymorphic {} with no receiver register",
            method.name
        ));
    };
    let kind = if method.class == "Ljava/lang/invoke/VarHandle;" {
        PolymorphicKind::VarHandle
    } else {
        PolymorphicKind::MethodHandle
    };
    let ptype = util::get_params_type(proto);
    let rtype = return_type_of(proto);

    let target = b.read_variable(receiver_reg)?;
    b.ir.value_mut(target)
        .refine_type(Some(kind.receiver_type()))?;
    let args = get_args(b, &ptype, arg_regs)?;
    let result = if rtype == "V" {
        None
    } else {
        Some(b.write_result_variable())
    };
    if let Some(result) = result {
        b.ir.value_mut(result).refine_type(Some(&rtype))?;
    }
    Ok(b.ir
        .add_ins(InsKind::InvokePolymorphic(PolymorphicInvokeData {
            target,
            kind,
            name: method.name.clone(),
            ptype,
            rtype,
            args,
            result,
        })))
}

/// The nine `method_handle_item.handle_type` values, by the names the smali text
/// writes them with (`invoke-static Lx;->y()V`).
const HANDLE_KINDS: [(&str, u8); 9] = [
    ("static-put", 0x00),
    ("static-get", 0x01),
    ("instance-put", 0x02),
    ("instance-get", 0x03),
    ("invoke-static", 0x04),
    ("invoke-instance", 0x05),
    ("invoke-direct", 0x06),
    ("invoke-constructor", 0x07),
    ("invoke-interface", 0x08),
];

/// Split a `const-method-handle` literal.
///
/// The literal is `Lclass;->name(proto)` for a method handle and
/// `Lclass;->name:type` for a field handle, optionally prefixed by the kind (the
/// smali *parser* accepts `invoke-static Lx;->y()V`; the dex *decoder* emits the
/// bare form, which is why [`kind_of_literal`] needs the dex's own table).
fn parse_handle_literal(literal: &str) -> Option<(u8, String, String, String)> {
    let literal = literal.trim().trim_matches('"').trim();
    let (kind, rest) = match HANDLE_KINDS
        .iter()
        .find(|(name, _)| literal.starts_with(name))
    {
        Some((name, kind)) => (
            *kind,
            literal[name.len()..]
                .trim_start_matches([' ', '@', ','])
                .trim(),
        ),
        None => (u8::MAX, literal),
    };
    let arrow = rest.find("->")?;
    let clsdesc = rest[..arrow].trim().to_string();
    let member = &rest[arrow + 2..];
    let (name, desc) = match member.find(|c| c == '(' || c == ':') {
        Some(at) if member.as_bytes()[at] == b'(' => (member[..at].trim(), member[at..].trim()),
        Some(at) => (member[..at].trim(), member[at + 1..].trim()),
        None => return None,
    };
    Some((kind, clsdesc, name.to_string(), desc.to_string()))
}

/// `const-method-type` / `const-method-handle`.
fn const_invoke_value(
    b: &mut IrBuilder,
    dest: &SmaliRegister,
    literal: Option<&str>,
    proto: Option<&str>,
) -> Result<InsId, String> {
    let value = b.write_variable(reg(b, dest));
    let cst = match (literal, proto) {
        (None, Some(proto)) => {
            b.ir.value_mut(value)
                .set_type(Some("Ljava/lang/invoke/MethodType;"));
            ConstInvokeValue::MethodType {
                proto: proto.to_string(),
            }
        }
        (Some(literal), _) => {
            let (inline_kind, clsdesc, name, desc) = parse_handle_literal(literal)
                .ok_or_else(|| format!("cannot parse method handle literal `{literal}`"))?;
            // The decoder renders a handle as `kind@Lclass;->name(desc)`; a
            // hand-written literal may leave the kind out, and then there is
            // nothing to recover it from (a field read and a field write of the
            // same field are two different handles for one literal).
            if inline_kind == u8::MAX {
                return Err(format!(
                    "const-method-handle `{literal}` does not name its kind; write it as \
                     `invoke-static@{literal}` (or static-put/static-get/instance-put/\
                     instance-get/invoke-instance/invoke-direct/invoke-constructor/\
                     invoke-interface)"
                ));
            }
            let kind = inline_kind;
            b.ir.value_mut(value)
                .set_type(Some("Ljava/lang/invoke/MethodHandle;"));
            ConstInvokeValue::MethodHandle {
                kind,
                clsdesc,
                name,
                desc,
            }
        }
        (None, None) => return Err("const-method-* without an operand".to_string()),
    };
    Ok(b.ir.add_ins(InsKind::ConstInvoke { value, cst }))
}

// ------------------------------------------------------------------ build ----

/// Build the IR for the instruction at index `idx` of `b.input`.
pub fn build(
    b: &mut IrBuilder,
    idx: usize,
    node: crate::ir::BlockId,
    op: &DexOp,
) -> Result<Option<InsId>, String> {
    use DexOp::*;
    let ins = match op {
        // 0x00 nop
        Nop => b.ir.add_ins(InsKind::Nop),

        // 0x01..0x09 moves
        Move { dest, src }
        | MoveFrom16 { dest, src }
        | Move16 { dest, src }
        | MoveWide { dest, src }
        | MoveWideFrom16 { dest, src }
        | MoveWide16 { dest, src }
        | MoveObject { dest, src }
        | MoveObjectFrom16 { dest, src }
        | MoveObject16 { dest, src } => {
            let rhs = b.read_variable(reg(b, src))?;
            let lhs = b.write_variable(reg(b, dest));
            b.ir.add_ins(InsKind::Move { lhs, rhs })
        }

        // 0x0a..0x0c move-result
        MoveResult { dest } | MoveResultWide { dest } | MoveResultObject { dest } => {
            let lhs = b.write_variable(reg(b, dest));
            let rhs = b.read_result_variable()?;
            b.ir.add_ins(InsKind::MoveResult { lhs, rhs })
        }

        // 0x0d move-exception
        MoveException { dest } => {
            let value = b.write_variable(reg(b, dest));
            let ty = b.ir.block(node).catch_type.clone().unwrap_or_default();
            b.ir.add_ins(InsKind::MoveException { value, ty })
        }

        // 0x0e..0x11 returns
        ReturnVoid => b.ir.add_ins(InsKind::Return {
            rtype: None,
            arg: None,
        }),
        Return { src } => {
            let arg = b.read_variable(reg(b, src))?;
            let rtype = b.get_return_type().to_string();
            b.ir.add_ins(InsKind::Return {
                rtype: Some(rtype),
                arg: Some(arg),
            })
        }
        ReturnWide { src } => {
            let arg = b.read_variable(reg(b, src))?;
            let rtype = b.get_return_type().to_string();
            b.ir.value_mut(arg).refine_type(Some(&rtype))?;
            b.ir.add_ins(InsKind::Return {
                rtype: Some(rtype),
                arg: Some(arg),
            })
        }
        ReturnObject { src } => {
            let arg = b.read_variable(reg(b, src))?;
            let rtype = b.get_return_type().to_string();
            b.ir.add_ins(InsKind::Return {
                rtype: Some(rtype),
                arg: Some(arg),
            })
        }

        // 0x12..0x19 constants
        Const4 { dest, value } => {
            let value_reg = b.write_variable(reg(b, dest));
            let cst = int_const(b, *value as i64, None)?;
            load_constant(b, value_reg, cst, false)?
        }
        Const16 { dest, value } => {
            let value_reg = b.write_variable(reg(b, dest));
            let cst = int_const(b, *value as i64, None)?;
            load_constant(b, value_reg, cst, false)?
        }
        Const { dest, value } => {
            let value_reg = b.write_variable(reg(b, dest));
            let cst = int_const(b, *value as i64, None)?;
            load_constant(b, value_reg, cst, false)?
        }
        ConstHigh16 { dest, value } => {
            let value_reg = b.write_variable(reg(b, dest));
            let cst = int_const(b, (*value as i64) << 16, None)?;
            load_constant(b, value_reg, cst, false)?
        }
        ConstWide16 { dest, value } => {
            let value_reg = b.write_variable(reg(b, dest));
            let cst = int_const(b, *value as i64, None)?;
            load_constant(b, value_reg, cst, false)?
        }
        ConstWide32 { dest, value } => {
            let value_reg = b.write_variable(reg(b, dest));
            let cst = int_const(b, *value as i64, None)?;
            load_constant(b, value_reg, cst, false)?
        }
        ConstWide { dest, value } => {
            let value_reg = b.write_variable(reg(b, dest));
            let cst = int_const(b, *value, None)?;
            load_constant(b, value_reg, cst, false)?
        }
        ConstWideHigh16 { dest, value } => {
            let value_reg = b.write_variable(reg(b, dest));
            let cst = int_const(b, (*value as i64) << 48, None)?;
            load_constant(b, value_reg, cst, false)?
        }

        // 0x1a..0x1b const-string
        ConstString { dest, value } | ConstStringJumbo { dest, value } => {
            let dest_reg = b.write_variable(reg(b, dest));
            b.ir.value_mut(dest_reg)
                .refine_type(Some("Ljava/lang/String;"))?;
            // `DexOp::ConstString.value` is *escaped smali text*, not the Java
            // string: the crate's own assembler runs `unescape_smali_string` on
            // it before writing the dex string table, and its `Display` writes it
            // back between quotes unescaped. Passing it through raw made the
            // generated code build a string containing a literal `\` + `n`, so a
            // real app's `UPDATE ...\n ...` SQL reached SQLite as
            // `\n        UPDATE ...` and failed with
            // `unrecognized token: "\" (code 1 SQLITE_ERROR)`. Every constant
            // holding an escape (`\n`, `\t`, `"`, `\\`, `\uXXXX`) was affected.
            let cst = str_const(
                b,
                smali::smali_ops::unescape_smali_string(&value),
                Some("Ljava/lang/String;"),
            )?;
            load_constant(b, dest_reg, cst, b.obfus)?
        }

        // 0x1c const-class
        ConstClass { dest, class } => {
            let dest_reg = b.write_variable(reg(b, dest));
            let cst = str_const(b, util::get_type(class), Some("Ljava/lang/Class;"))?;
            load_constant(b, dest_reg, cst, false)?
        }

        // 0x1d..0x1e monitors
        MonitorEnter { src } => {
            let r = b.read_variable(reg(b, src))?;
            b.ir.add_ins(InsKind::MonitorEnter { r })
        }
        MonitorExit { src } => {
            let r = b.read_variable(reg(b, src))?;
            b.ir.add_ins(InsKind::MonitorExit { r })
        }

        // 0x1f check-cast
        CheckCast { dest, class } => {
            let arg = b.read_variable(reg(b, dest))?;
            b.ir.add_ins(InsKind::CheckCast {
                arg,
                clsdesc: class.clone(),
            })
        }

        // 0x20 instance-of
        InstanceOf { dest, src, class } => {
            let obj = b.read_variable(reg(b, src))?;
            let value = b.write_variable(reg(b, dest));
            b.ir.add_ins(InsKind::InstanceOf {
                value,
                obj,
                clsdesc: class.clone(),
            })
        }

        // 0x21 array-length
        ArrayLength { dest, array } => {
            let array_val = b.read_variable(reg(b, array))?;
            let value = b.write_variable(reg(b, dest));
            b.ir.add_ins(InsKind::ArrayLength {
                value,
                array: array_val,
            })
        }

        // 0x22 new-instance
        NewInstance { dest, class } => {
            let value = b.write_variable(reg(b, dest));
            b.ir.add_ins(InsKind::NewInstance {
                value,
                ins_type: class.clone(),
            })
        }

        // 0x23 new-array
        NewArray {
            dest,
            size_reg,
            class,
        } => {
            let size = b.read_variable(reg(b, size_reg))?;
            let value = b.write_variable(reg(b, dest));
            b.ir.add_ins(InsKind::NewArray {
                value,
                size,
                atype: class.clone(),
            })
        }

        // 0x24..0x25 filled-new-array
        FilledNewArray { registers, class } => {
            let regs = registers.iter().map(|r| reg(b, r)).collect::<Vec<_>>();
            let size = regs.len() as u32;
            let args = get_variables(b, &regs)?;
            let value = b.write_result_variable();
            b.ir.add_ins(InsKind::FilledArray {
                value,
                size,
                atype: class.clone(),
                args,
            })
        }
        FilledNewArrayRange { registers, class } => {
            let regs = range_regs(b, registers);
            let size = regs.len() as u32;
            let args = get_variables(b, &regs)?;
            let value = b.write_result_variable();
            b.ir.add_ins(InsKind::FilledArray {
                value,
                size,
                atype: class.clone(),
                args,
            })
        }

        // 0x26 fill-array-data
        FillArrayData { reg: array, offset } => {
            let name = offset.0.strip_prefix(':').unwrap_or(&offset.0).to_string();
            let data = match b.input.payloads.get(&name) {
                Some(Payload::Array(data)) => data.clone(),
                _ => {
                    return Err(format!(
                        "missing array-data payload :{name} in {}{}",
                        b.input.name, b.input.proto
                    ));
                }
            };
            let reg_val = b.read_variable(reg(b, array))?;
            b.ir.add_ins(InsKind::FillArray { reg: reg_val, data })
        }

        // 0x27 throw
        Throw { src } => {
            let r = b.read_variable(reg(b, src))?;
            b.ir.add_ins(InsKind::Throw { r })
        }

        // 0x28..0x2a goto
        Goto { offset } | Goto16 { offset } | Goto32 { offset } => {
            let target = label_index(b, offset)?;
            b.ir.add_ins(InsKind::Goto { target })
        }

        // 0x2b..0x2c switches
        PackedSwitch {
            reg: selector,
            offset,
        }
        | SparseSwitch {
            reg: selector,
            offset,
        } => {
            let name = offset.0.strip_prefix(':').unwrap_or(&offset.0).to_string();
            let cases = match b.input.payloads.get(&name) {
                Some(Payload::Packed { first_key, targets }) => targets
                    .iter()
                    .enumerate()
                    .map(|(i, t)| (*first_key + i as i32, *t))
                    .collect::<Vec<(i32, LocIdx)>>(),
                Some(Payload::Sparse(entries)) => entries.clone(),
                _ => {
                    return Err(format!(
                        "missing switch payload :{name} in {}{}",
                        b.input.name, b.input.proto
                    ));
                }
            };
            let src = b.read_variable(reg(b, selector))?;
            b.ir.add_ins(InsKind::Switch { src, cases })
        }

        // 0x2d..0x31 floating/long compares
        CmplFloat { dest, src1, src2 } => binary_comp(
            b,
            BinOp::Cmpl,
            reg(b, dest),
            reg(b, src1),
            reg(b, src2),
            "F",
        )?,
        CmpgFloat { dest, src1, src2 } => binary_comp(
            b,
            BinOp::Cmpg,
            reg(b, dest),
            reg(b, src1),
            reg(b, src2),
            "F",
        )?,
        CmplDouble { dest, src1, src2 } => binary_comp(
            b,
            BinOp::Cmpl,
            reg(b, dest),
            reg(b, src1),
            reg(b, src2),
            "D",
        )?,
        CmpgDouble { dest, src1, src2 } => binary_comp(
            b,
            BinOp::Cmpg,
            reg(b, dest),
            reg(b, src1),
            reg(b, src2),
            "D",
        )?,
        CmpLong { dest, src1, src2 } => {
            binary_comp(b, BinOp::Cmp, reg(b, dest), reg(b, src1), reg(b, src2), "J")?
        }

        // 0x32..0x37 if-*
        IfEq { reg1, reg2, offset } => cond(
            b,
            CondOp::Equal,
            reg(b, reg1),
            reg(b, reg2),
            label_index(b, offset)?,
        )?,
        IfNe { reg1, reg2, offset } => cond(
            b,
            CondOp::NEqual,
            reg(b, reg1),
            reg(b, reg2),
            label_index(b, offset)?,
        )?,
        IfLt { reg1, reg2, offset } => cond(
            b,
            CondOp::Lower,
            reg(b, reg1),
            reg(b, reg2),
            label_index(b, offset)?,
        )?,
        IfGe { reg1, reg2, offset } => cond(
            b,
            CondOp::GEqual,
            reg(b, reg1),
            reg(b, reg2),
            label_index(b, offset)?,
        )?,
        IfGt { reg1, reg2, offset } => cond(
            b,
            CondOp::Greater,
            reg(b, reg1),
            reg(b, reg2),
            label_index(b, offset)?,
        )?,
        IfLe { reg1, reg2, offset } => cond(
            b,
            CondOp::LEqual,
            reg(b, reg1),
            reg(b, reg2),
            label_index(b, offset)?,
        )?,

        // 0x38..0x3d if-*z
        IfEqz { reg: r, offset } => condz(b, CondOp::Equal, reg(b, r), label_index(b, offset)?)?,
        IfNez { reg: r, offset } => condz(b, CondOp::NEqual, reg(b, r), label_index(b, offset)?)?,
        IfLtz { reg: r, offset } => condz(b, CondOp::Lower, reg(b, r), label_index(b, offset)?)?,
        IfGez { reg: r, offset } => {
            let value = reg(b, r);
            let arg = b.read_variable(value)?;
            b.ir.value_mut(arg).refine_type(Some("I"))?;
            b.ir.add_ins(InsKind::CondZ {
                op: CondOp::GEqual,
                arg,
                target: label_index(b, offset)?,
            })
        }
        IfGtz { reg: r, offset } => condz(b, CondOp::Greater, reg(b, r), label_index(b, offset)?)?,
        IfLez { reg: r, offset } => condz(b, CondOp::LEqual, reg(b, r), label_index(b, offset)?)?,

        // 0x44..0x4a aget*
        AGet { dest, array, index } => {
            array_load(b, reg(b, dest), reg(b, array), reg(b, index), None)?
        }
        AGetWide { dest, array, index } => {
            array_load(b, reg(b, dest), reg(b, array), reg(b, index), None)?
        }
        AGetObject { dest, array, index } => {
            array_load(b, reg(b, dest), reg(b, array), reg(b, index), None)?
        }
        AGetBoolean { dest, array, index } => {
            array_load(b, reg(b, dest), reg(b, array), reg(b, index), Some("Z"))?
        }
        AGetByte { dest, array, index } => {
            array_load(b, reg(b, dest), reg(b, array), reg(b, index), Some("B"))?
        }
        AGetChar { dest, array, index } => {
            array_load(b, reg(b, dest), reg(b, array), reg(b, index), Some("C"))?
        }
        AGetShort { dest, array, index } => {
            array_load(b, reg(b, dest), reg(b, array), reg(b, index), Some("S"))?
        }

        // 0x4b..0x51 aput*
        APut { src, array, index } => {
            array_store(b, reg(b, src), reg(b, array), reg(b, index), None)?
        }
        APutWide { src, array, index } => {
            array_store(b, reg(b, src), reg(b, array), reg(b, index), None)?
        }
        APutObject { src, array, index } => {
            array_store(b, reg(b, src), reg(b, array), reg(b, index), None)?
        }
        APutBoolean { src, array, index } => {
            array_store(b, reg(b, src), reg(b, array), reg(b, index), Some("Z"))?
        }
        APutByte { src, array, index } => {
            array_store(b, reg(b, src), reg(b, array), reg(b, index), Some("B"))?
        }
        APutChar { src, array, index } => {
            array_store(b, reg(b, src), reg(b, array), reg(b, index), Some("C"))?
        }
        APutShort { src, array, index } => {
            array_store(b, reg(b, src), reg(b, array), reg(b, index), Some("S"))?
        }

        // 0x52..0x58 iget*
        IGet {
            dest,
            object,
            field,
        }
        | IGetWide {
            dest,
            object,
            field,
        }
        | IGetObject {
            dest,
            object,
            field,
        }
        | IGetBoolean {
            dest,
            object,
            field,
        }
        | IGetByte {
            dest,
            object,
            field,
        }
        | IGetChar {
            dest,
            object,
            field,
        }
        | IGetShort {
            dest,
            object,
            field,
        } => {
            let (clsdesc, ftype, name) = get_field(field);
            let arg = b.read_variable(reg(b, object))?;
            let value = b.write_variable(reg(b, dest));
            b.ir.add_ins(InsKind::GetInstance {
                value,
                arg,
                ftype,
                clsdesc,
                name,
            })
        }

        // 0x59..0x5f iput*
        IPut { src, object, field }
        | IPutWide { src, object, field }
        | IPutObject { src, object, field }
        | IPutBoolean { src, object, field }
        | IPutByte { src, object, field }
        | IPutChar { src, object, field }
        | IPutShort { src, object, field } => {
            let (clsdesc, atype, name) = get_field(field);
            let lhs = b.read_variable(reg(b, object))?;
            let rhs = b.read_variable(reg(b, src))?;
            b.ir.add_ins(InsKind::InstancePut {
                lhs,
                rhs,
                clsdesc,
                atype,
                name,
            })
        }

        // 0x60..0x66 sget*
        SGet { dest, field }
        | SGetWide { dest, field }
        | SGetObject { dest, field }
        | SGetBoolean { dest, field }
        | SGetByte { dest, field }
        | SGetChar { dest, field }
        | SGetShort { dest, field } => {
            let (clsdesc, atype, name) = get_field(field);
            let value = b.write_variable(reg(b, dest));
            b.ir.value_mut(value).refine_type(Some(&atype))?;
            b.ir.add_ins(InsKind::GetStatic {
                value,
                ftype: atype,
                clsdesc,
                name,
            })
        }

        // 0x67..0x6d sput*
        SPut { src, field }
        | SPutWide { src, field }
        | SPutObject { src, field }
        | SPutBoolean { src, field }
        | SPutByte { src, field }
        | SPutChar { src, field }
        | SPutShort { src, field } => {
            let (clsdesc, ftype, name) = get_field(field);
            let rhs = b.read_variable(reg(b, src))?;
            b.ir.add_ins(InsKind::StaticPut {
                rhs,
                clsdesc,
                ftype,
                name,
            })
        }

        // 0x6e..0x72 invokes
        InvokeVirtual { registers, method } => {
            let regs: Vec<i64> = registers.iter().map(|r| reg(b, r)).collect();
            let (thiz, args) = regs.split_first().ok_or("invoke with no registers")?;
            invoke_common(b, InvokeType::Virtual, method, Some(*thiz), args)?
        }
        InvokeSuper { registers, method } => {
            let regs: Vec<i64> = registers.iter().map(|r| reg(b, r)).collect();
            let (thiz, args) = regs.split_first().ok_or("invoke with no registers")?;
            invoke_common(b, InvokeType::Super, method, Some(*thiz), args)?
        }
        InvokeDirect { registers, method } => {
            let regs: Vec<i64> = registers.iter().map(|r| reg(b, r)).collect();
            let (thiz, args) = regs.split_first().ok_or("invoke with no registers")?;
            invoke_common(b, InvokeType::Direct, method, Some(*thiz), args)?
        }
        InvokeStatic { registers, method } => {
            let regs: Vec<i64> = registers.iter().map(|r| reg(b, r)).collect();
            invoke_common(b, InvokeType::Static, method, None, &regs)?
        }
        InvokeInterface { registers, method } => {
            let regs: Vec<i64> = registers.iter().map(|r| reg(b, r)).collect();
            let (thiz, args) = regs.split_first().ok_or("invoke with no registers")?;
            invoke_common(b, InvokeType::Interface, method, Some(*thiz), args)?
        }

        // 0x74..0x78 invoke-*/range
        InvokeVirtualRange { range, method } => {
            let regs = range_regs(b, range);
            let (thiz, args) = regs.split_first().ok_or("invoke with no registers")?;
            invoke_common(b, InvokeType::Virtual, method, Some(*thiz), args)?
        }
        InvokeSuperRange { range, method } => {
            let regs = range_regs(b, range);
            let (thiz, args) = regs.split_first().ok_or("invoke with no registers")?;
            invoke_common(b, InvokeType::Super, method, Some(*thiz), args)?
        }
        InvokeDirectRange { range, method } => {
            let regs = range_regs(b, range);
            let (thiz, args) = regs.split_first().ok_or("invoke with no registers")?;
            invoke_common(b, InvokeType::Direct, method, Some(*thiz), args)?
        }
        InvokeStaticRange { range, method } => {
            let regs = range_regs(b, range);
            invoke_common(b, InvokeType::Static, method, None, &regs)?
        }
        InvokeInterfaceRange { range, method } => {
            let regs = range_regs(b, range);
            let (thiz, args) = regs.split_first().ok_or("invoke with no registers")?;
            invoke_common(b, InvokeType::Interface, method, Some(*thiz), args)?
        }

        // 0x7b..0x81 unary
        NegInt { dest, src } => unary(b, reg(b, dest), UnOp::Neg, reg(b, src), "I")?,
        NotInt { dest, src } => unary(b, reg(b, dest), UnOp::Not, reg(b, src), "I")?,
        NegLong { dest, src } => unary(b, reg(b, dest), UnOp::Neg, reg(b, src), "J")?,
        NotLong { dest, src } => unary(b, reg(b, dest), UnOp::Not, reg(b, src), "J")?,
        NegFloat { dest, src } => unary(b, reg(b, dest), UnOp::Neg, reg(b, src), "F")?,
        NegDouble { dest, src } => unary(b, reg(b, dest), UnOp::Neg, reg(b, src), "D")?,

        // 0x82..0x8f conversions
        IntToLong { dest, src } => cast(b, reg(b, dest), "(jlong)", "J", reg(b, src), "I")?,
        IntToFloat { dest, src } => cast(b, reg(b, dest), "(jfloat)", "F", reg(b, src), "I")?,
        IntToDouble { dest, src } => cast(b, reg(b, dest), "(jdouble)", "D", reg(b, src), "I")?,
        LongToInt { dest, src } => cast(b, reg(b, dest), "(jint)", "I", reg(b, src), "J")?,
        LongToFloat { dest, src } => cast(b, reg(b, dest), "(jfloat)", "F", reg(b, src), "J")?,
        LongToDouble { dest, src } => cast(b, reg(b, dest), "(jdouble)", "D", reg(b, src), "J")?,
        FloatToInt { dest, src } => cast(b, reg(b, dest), "(jint)", "I", reg(b, src), "F")?,
        FloatToLong { dest, src } => cast(b, reg(b, dest), "(jlong)", "J", reg(b, src), "F")?,
        FloatToDouble { dest, src } => cast(b, reg(b, dest), "(jdouble)", "D", reg(b, src), "F")?,
        DoubleToInt { dest, src } => cast(b, reg(b, dest), "(jint)", "I", reg(b, src), "D")?,
        DoubleToLong { dest, src } => cast(b, reg(b, dest), "(jlong)", "J", reg(b, src), "D")?,
        DoubleToFloat { dest, src } => cast(b, reg(b, dest), "(jfloat)", "F", reg(b, src), "D")?,
        IntToByte { dest, src } => cast(b, reg(b, dest), "(jbyte)", "B", reg(b, src), "I")?,
        IntToChar { dest, src } => cast(b, reg(b, dest), "(jchar)", "C", reg(b, src), "I")?,
        IntToShort { dest, src } => cast(b, reg(b, dest), "(jshort)", "S", reg(b, src), "I")?,

        // 0x90..0xaf binary arithmetic
        AddInt { dest, src1, src2 } => {
            assign_binary(b, BinOp::Add, "I", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        SubInt { dest, src1, src2 } => {
            assign_binary(b, BinOp::Sub, "I", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        MulInt { dest, src1, src2 } => {
            assign_binary(b, BinOp::Mul, "I", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        DivInt { dest, src1, src2 } => {
            assign_binary(b, BinOp::Div, "I", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        RemInt { dest, src1, src2 } => {
            assign_binary(b, BinOp::Mod, "I", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        AndInt { dest, src1, src2 } => {
            assign_binary(b, BinOp::And, "I", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        OrInt { dest, src1, src2 } => {
            assign_binary(b, BinOp::Or, "I", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        XorInt { dest, src1, src2 } => {
            assign_binary(b, BinOp::Xor, "I", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        ShlInt { dest, src1, src2 } => assign_binary(
            b,
            BinOp::InShl,
            "I",
            reg(b, dest),
            reg(b, src1),
            reg(b, src2),
        )?,
        ShrInt { dest, src1, src2 } => assign_binary(
            b,
            BinOp::InShr,
            "I",
            reg(b, dest),
            reg(b, src1),
            reg(b, src2),
        )?,
        UshrInt { dest, src1, src2 } => assign_binary(
            b,
            BinOp::InUShr,
            "I",
            reg(b, dest),
            reg(b, src1),
            reg(b, src2),
        )?,
        AddLong { dest, src1, src2 } => {
            assign_binary(b, BinOp::Add, "J", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        SubLong { dest, src1, src2 } => {
            assign_binary(b, BinOp::Sub, "J", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        MulLong { dest, src1, src2 } => {
            assign_binary(b, BinOp::Mul, "J", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        DivLong { dest, src1, src2 } => {
            assign_binary(b, BinOp::Div, "J", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        RemLong { dest, src1, src2 } => {
            assign_binary(b, BinOp::Mod, "J", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        AndLong { dest, src1, src2 } => {
            assign_binary(b, BinOp::And, "J", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        OrLong { dest, src1, src2 } => {
            assign_binary(b, BinOp::Or, "J", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        XorLong { dest, src1, src2 } => {
            assign_binary(b, BinOp::Xor, "J", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        ShlLong { dest, src1, src2 } => assign_binary(
            b,
            BinOp::LongShl,
            "J",
            reg(b, dest),
            reg(b, src1),
            reg(b, src2),
        )?,
        ShrLong { dest, src1, src2 } => assign_binary(
            b,
            BinOp::LongShr,
            "J",
            reg(b, dest),
            reg(b, src1),
            reg(b, src2),
        )?,
        UshrLong { dest, src1, src2 } => assign_binary(
            b,
            BinOp::LongUShr,
            "J",
            reg(b, dest),
            reg(b, src1),
            reg(b, src2),
        )?,
        AddFloat { dest, src1, src2 } => {
            assign_binary(b, BinOp::Add, "F", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        SubFloat { dest, src1, src2 } => {
            assign_binary(b, BinOp::Sub, "F", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        MulFloat { dest, src1, src2 } => {
            assign_binary(b, BinOp::Mul, "F", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        DivFloat { dest, src1, src2 } => {
            assign_binary(b, BinOp::Div, "F", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        RemFloat { dest, src1, src2 } => assign_binary(
            b,
            BinOp::ModF,
            "F",
            reg(b, dest),
            reg(b, src1),
            reg(b, src2),
        )?,
        AddDouble { dest, src1, src2 } => {
            assign_binary(b, BinOp::Add, "D", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        SubDouble { dest, src1, src2 } => {
            assign_binary(b, BinOp::Sub, "D", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        MulDouble { dest, src1, src2 } => {
            assign_binary(b, BinOp::Mul, "D", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        DivDouble { dest, src1, src2 } => {
            assign_binary(b, BinOp::Div, "D", reg(b, dest), reg(b, src1), reg(b, src2))?
        }
        RemDouble { dest, src1, src2 } => assign_binary(
            b,
            BinOp::ModD,
            "D",
            reg(b, dest),
            reg(b, src1),
            reg(b, src2),
        )?,

        // 0xb0..0xcf arithmetic /2addr
        AddInt2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::Add, "I", reg(b, dest), reg(b, src))?
        }
        SubInt2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::Sub, "I", reg(b, dest), reg(b, src))?
        }
        MulInt2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::Mul, "I", reg(b, dest), reg(b, src))?
        }
        DivInt2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::Div, "I", reg(b, dest), reg(b, src))?
        }
        RemInt2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::Mod, "I", reg(b, dest), reg(b, src))?
        }
        AndInt2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::And, "I", reg(b, dest), reg(b, src))?
        }
        OrInt2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::Or, "I", reg(b, dest), reg(b, src))?
        }
        XorInt2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::Xor, "I", reg(b, dest), reg(b, src))?
        }
        ShlInt2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::InShl, "I", reg(b, dest), reg(b, src))?
        }
        ShrInt2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::InShr, "I", reg(b, dest), reg(b, src))?
        }
        UshrInt2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::InUShr, "I", reg(b, dest), reg(b, src))?
        }
        AddLong2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::Add, "J", reg(b, dest), reg(b, src))?
        }
        SubLong2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::Sub, "J", reg(b, dest), reg(b, src))?
        }
        MulLong2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::Mul, "J", reg(b, dest), reg(b, src))?
        }
        DivLong2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::Div, "J", reg(b, dest), reg(b, src))?
        }
        RemLong2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::Mod, "J", reg(b, dest), reg(b, src))?
        }
        AndLong2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::And, "J", reg(b, dest), reg(b, src))?
        }
        OrLong2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::Or, "J", reg(b, dest), reg(b, src))?
        }
        XorLong2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::Xor, "J", reg(b, dest), reg(b, src))?
        }
        ShlLong2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::LongShl, "J", reg(b, dest), reg(b, src))?
        }
        ShrLong2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::LongShr, "J", reg(b, dest), reg(b, src))?
        }
        UshrLong2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::LongUShr, "J", reg(b, dest), reg(b, src))?
        }
        AddFloat2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::Add, "F", reg(b, dest), reg(b, src))?
        }
        SubFloat2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::Sub, "F", reg(b, dest), reg(b, src))?
        }
        MulFloat2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::Mul, "F", reg(b, dest), reg(b, src))?
        }
        DivFloat2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::Div, "F", reg(b, dest), reg(b, src))?
        }
        RemFloat2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::ModF, "F", reg(b, dest), reg(b, src))?
        }
        AddDouble2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::Add, "D", reg(b, dest), reg(b, src))?
        }
        SubDouble2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::Sub, "D", reg(b, dest), reg(b, src))?
        }
        MulDouble2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::Mul, "D", reg(b, dest), reg(b, src))?
        }
        DivDouble2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::Div, "D", reg(b, dest), reg(b, src))?
        }
        RemDouble2Addr { reg: dest, src } => {
            assign_binary_2addr(b, BinOp::ModD, "D", reg(b, dest), reg(b, src))?
        }

        // 0xd0..0xd7 /lit16
        AddIntLit16 { dest, src, literal } => {
            assign_lit(b, BinOp::Add, *literal as i64, reg(b, dest), reg(b, src))?
        }
        RSubIntLit16 { dest, src, literal } => {
            let arg = b.read_variable(reg(b, src))?;
            let value = b.write_variable(reg(b, dest));
            b.ir.value_mut(value).refine_type(Some("I"))?;
            let cst = int_const(b, *literal as i64, Some("I"))?;
            b.ir.add_ins(InsKind::Binary {
                op: BinOp::Sub,
                value,
                arg1: cst,
                arg2: arg,
                op_type: "I".to_string(),
                comp: false,
            })
        }
        MulIntLit16 { dest, src, literal } => {
            assign_lit(b, BinOp::Mul, *literal as i64, reg(b, dest), reg(b, src))?
        }
        DivIntLit16 { dest, src, literal } => {
            assign_lit(b, BinOp::Div, *literal as i64, reg(b, dest), reg(b, src))?
        }
        RemIntLit16 { dest, src, literal } => {
            assign_lit(b, BinOp::Mod, *literal as i64, reg(b, dest), reg(b, src))?
        }
        AndIntLit16 { dest, src, literal } => {
            assign_lit(b, BinOp::And, *literal as i64, reg(b, dest), reg(b, src))?
        }
        OrIntLit16 { dest, src, literal } => {
            assign_lit(b, BinOp::Or, *literal as i64, reg(b, dest), reg(b, src))?
        }
        XorIntLit16 { dest, src, literal } => {
            assign_lit(b, BinOp::Xor, *literal as i64, reg(b, dest), reg(b, src))?
        }

        // 0xd8..0xe2 /lit8
        AddIntLit8 { dest, src, literal } => {
            // `add-int/lit8` flips to a subtraction for a negative literal.
            let (literal, op) = if *literal < 0 {
                (-(*literal as i64), BinOp::Sub)
            } else {
                (*literal as i64, BinOp::Add)
            };
            let arg = b.read_variable(reg(b, src))?;
            let value = b.write_variable(reg(b, dest));
            let cst = int_const(b, literal, Some("B"))?;
            b.ir.add_ins(InsKind::Binary {
                op,
                value,
                arg1: arg,
                arg2: cst,
                op_type: "I".to_string(),
                comp: false,
            })
        }
        RSubIntLit8 { dest, src, literal } => {
            let arg = b.read_variable(reg(b, src))?;
            let value = b.write_variable(reg(b, dest));
            b.ir.value_mut(value).refine_type(Some("I"))?;
            let cst = int_const(b, *literal as i64, Some("I"))?;
            b.ir.add_ins(InsKind::Binary {
                op: BinOp::Sub,
                value,
                arg1: cst,
                arg2: arg,
                op_type: "I".to_string(),
                comp: false,
            })
        }
        MulIntLit8 { dest, src, literal } => {
            assign_lit(b, BinOp::Mul, *literal as i64, reg(b, dest), reg(b, src))?
        }
        DivIntLit8 { dest, src, literal } => {
            assign_lit(b, BinOp::Div, *literal as i64, reg(b, dest), reg(b, src))?
        }
        RemIntLit8 { dest, src, literal } => {
            assign_lit(b, BinOp::Mod, *literal as i64, reg(b, dest), reg(b, src))?
        }
        AndIntLit8 { dest, src, literal } => {
            assign_lit(b, BinOp::And, *literal as i64, reg(b, dest), reg(b, src))?
        }
        OrIntLit8 { dest, src, literal } => {
            assign_lit(b, BinOp::Or, *literal as i64, reg(b, dest), reg(b, src))?
        }
        XorIntLit8 { dest, src, literal } => {
            assign_lit(b, BinOp::Xor, *literal as i64, reg(b, dest), reg(b, src))?
        }
        ShlIntLit8 { dest, src, literal } => {
            assign_lit(b, BinOp::InShl, *literal as i64, reg(b, dest), reg(b, src))?
        }
        ShrIntLit8 { dest, src, literal } => {
            assign_lit(b, BinOp::InShr, *literal as i64, reg(b, dest), reg(b, src))?
        }
        UshrIntLit8 { dest, src, literal } => {
            assign_lit(b, BinOp::InUShr, *literal as i64, reg(b, dest), reg(b, src))?
        }

        // Instructions the Python tool had no implementation for: the method is
        // skipped by the caller, exactly as `INSTRUCTION_SET[opcode]` raised
        // `IndexError`.
        Unused { opcode } => {
            return Err(format!("unsupported opcode 0x{opcode:02x}"));
        }
        InvokePolymorphic {
            registers,
            method,
            proto,
        } => {
            let regs: Vec<i64> = registers.iter().map(|r| reg(b, r)).collect();
            polymorphic_invoke(b, method, proto, &regs)?
        }
        InvokePolymorphicRange {
            range,
            method,
            proto,
        } => {
            let regs = range_regs(b, range);
            polymorphic_invoke(b, method, proto, &regs)?
        }

        // `invoke-custom` needs its call site's *static arguments* (the
        // `call_site_item` array), and this front-end does not carry them: the
        // decoder resolves the call-site index into a string holding only the
        // bootstrap method and the call-site name/prototype. Those arguments are
        // not decoration -- a `StringConcatFactory` recipe and a
        // `LambdaMetafactory` `samMethodType`/`implMethod` are exactly what the
        // bootstrap needs to build the target -- so lowering the instruction
        // without them would silently produce a call site that does something
        // else. D8 desugars both away for every `--min-api`, which is why this
        // is rare in real APKs; `--no-desugaring` output is what still has it.
        // Refusing names the problem instead of half-implementing it.
        InvokeCustom { .. } | InvokeCustomRange { .. } => {
            return Err(format!(
                "unsupported instruction {op}: invoke-custom needs the call site's \
                 static arguments, which this front-end does not expose (D8 \
                 desugars string concatenation and lambdas unless built with \
                 --no-desugaring)"
            ));
        }

        // `const-method-type` and `const-method-handle`: one runtime call each
        // (`MethodType.methodType` / `MethodHandles.Lookup`). The handle's kind
        // comes from the literal when the input names it, and otherwise from the
        // dex's own `method_handles` table, which the caller supplies.
        ConstMethodType { dest, proto } => const_invoke_value(b, dest, None, Some(proto))?,
        ConstMethodHandle {
            dest,
            method_handle,
        } => const_invoke_value(b, dest, Some(method_handle), None)?,

        // Quickened (`execute-inline`) opcodes exist only in an on-device
        // optimised dex/vdex, never in a shipped APK's `.dex`.
        ExecuteInline { .. } | ExecuteInlineRange { .. } => {
            return Err(format!(
                "unsupported instruction {op}: execute-inline is an ODEX-only opcode"
            ));
        }
        ThrowVerificationError { .. } => {
            return Err("unsupported instruction throw-verification-error".to_string());
        }
    };

    let _ = idx;
    Ok(Some(ins))
}
