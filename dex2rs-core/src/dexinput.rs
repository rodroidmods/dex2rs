//! Decoding bridge: `SmaliMethod` (from the `smali` crate) -> a linear,
//! label-resolved instruction list the rest of the compiler works on.
//!
//! The smali crate hands us `SmaliOp`s in source order: labels, real
//! instructions, payload directives (`.array-data` / `.packed-switch` /
//! `.sparse-switch`) and, appended at the end, the `.catch` directives. This
//! module flattens that into:
//!
//! * `ops` — the instructions, in order, each with its own index ([`LocIdx`]),
//! * `labels` — smali label -> index of the instruction it precedes,
//! * `payloads` — payload label -> decoded switch/array payload,
//! * `exceptions` — try/catch ranges as `[start, end)` index ranges.

use std::collections::BTreeMap;

use smali::smali_ops::DexOp;
use smali::types::{
    ArrayDataDirective, ArrayDataElement, CatchDirective, Modifier, SmaliClass, SmaliMethod,
    SmaliOp,
};

use crate::ir::{ArrayData, LocIdx};
use crate::util;

/// One decoded instruction plus the text used by the `LOGD` trace.
#[derive(Clone, Debug)]
pub struct DecodedOp {
    pub op: DexOp,
    pub mnemonic: String,
    pub operands: String,
}

#[derive(Clone, Debug)]
pub enum Payload {
    Array(ArrayData),
    Packed {
        first_key: i32,
        targets: Vec<LocIdx>,
    },
    Sparse(Vec<(i32, LocIdx)>),
}

/// A try range and its handlers, typed handlers first and the catch-all last —
/// the order ART tests them in.
#[derive(Clone, Debug)]
pub struct ExceptionRange {
    pub start: LocIdx,
    /// Exclusive.
    pub end: LocIdx,
    pub handlers: Vec<(String, LocIdx)>,
}

/// Everything the compiler needs about one method.
#[derive(Clone, Debug)]
pub struct MethodInput {
    /// JNI class descriptor, e.g. `Lcom/example/Foo;`.
    pub class_name: String,
    pub name: String,
    /// JNI prototype, e.g. `(ILjava/lang/String;)V`.
    pub proto: String,
    pub access: u32,
    pub is_static: bool,
    pub locals: u32,
    pub ins_size: u32,
    pub registers_size: u32,
    pub ops: Vec<DecodedOp>,
    pub labels: BTreeMap<String, LocIdx>,
    pub payloads: BTreeMap<String, Payload>,
    pub exceptions: Vec<ExceptionRange>,
    pub obfus: bool,
    pub dynamic_register: bool,
    /// `LOGD`-style tracing of every instruction; off by default (see
    /// [`crate::ir::Ir::trace`]).
    pub trace: bool,
}

impl MethodInput {
    pub fn return_type(&self) -> String {
        match self.proto.find(')') {
            Some(i) => self.proto[i + 1..].to_string(),
            None => "V".to_string(),
        }
    }

    pub fn params_type(&self) -> Vec<String> {
        util::get_params_type(&self.proto)
    }

    /// `MethodFilter`-visible triple: `(class, name, proto)`.
    pub fn triple(&self) -> (String, String, String) {
        (
            self.class_name.clone(),
            self.name.clone(),
            self.proto.clone(),
        )
    }

    /// `JniLongName()` for this method.
    pub fn jni_long_name(&self) -> String {
        util::jni_long_name(&self.class_name, &self.name, &self.proto)
    }

    /// Absolute registers holding the incoming arguments (`lparams` in Python),
    /// `this` first for instance methods. Wide parameters occupy two registers.
    ///
    /// This mirrors `Dex2C.__init__`: the `this` register is recorded even when
    /// the method takes no parameters.
    pub fn param_registers(&self, params_type: &[String]) -> Vec<i64> {
        let mut out = Vec::new();
        let mut register = self.registers_size as i64 - self.ins_size as i64;
        if !self.is_static {
            out.push(register);
            register += 1;
        }
        for ptype in params_type {
            out.push(register);
            register += util::get_type_size(ptype);
        }
        out
    }
}

/// Resolve a smali register (`vN` / `pN`) to its absolute index in the method's
/// register file, mirroring the smali crate's own `resolve_register`.
pub fn resolve_register(
    reg: &smali::smali_ops::SmaliRegister,
    registers_size: u32,
    ins_size: u32,
) -> i64 {
    match reg {
        smali::smali_ops::SmaliRegister::Local(i) => *i as i64,
        smali::smali_ops::SmaliRegister::Parameter(i) => {
            (registers_size as i64).saturating_sub(ins_size as i64) + *i as i64
        }
    }
}

fn strip_colon(label: &str) -> String {
    label.strip_prefix(':').unwrap_or(label).to_string()
}

fn array_data_bytes(dir: &ArrayDataDirective) -> ArrayData {
    let mut bytes = Vec::new();
    for e in &dir.elements {
        match e {
            ArrayDataElement::Byte(b) => bytes.push(*b as u8),
            ArrayDataElement::Short(s) => bytes.extend_from_slice(&s.to_le_bytes()),
            ArrayDataElement::Int(i) => bytes.extend_from_slice(&i.to_le_bytes()),
            ArrayDataElement::Long(l) => bytes.extend_from_slice(&l.to_le_bytes()),
            ArrayDataElement::Float(f) => bytes.extend_from_slice(&f.to_bits().to_le_bytes()),
            ArrayDataElement::Double(d) => bytes.extend_from_slice(&d.to_bits().to_le_bytes()),
        }
    }
    ArrayData {
        element_width: dir.element_width,
        bytes,
    }
}

/// Convert smali modifiers back into dex access flags (only the bits that
/// survive `DexFile::to_smali`).
pub fn modifiers_to_access(modifiers: &[Modifier], constructor: bool) -> u32 {
    let mut access = 0u32;
    for m in modifiers {
        access |= match m {
            Modifier::Public => 0x1,
            Modifier::Private => 0x2,
            Modifier::Protected => 0x4,
            Modifier::Static => 0x8,
            Modifier::Final => 0x10,
            Modifier::Synchronized => 0x20,
            Modifier::Bridge => 0x40,
            Modifier::Varargs => 0x80,
            Modifier::Native => 0x100,
            Modifier::Interface => 0x200,
            Modifier::Abstract => 0x400,
            Modifier::Strict => 0x800,
            Modifier::Synthetic => 0x1000,
            Modifier::Annotation => 0x2000,
            Modifier::Enum => 0x4000,
            Modifier::DeclaredSynchronized => 0x20000,
            Modifier::Volatile => 0x40,
            Modifier::Transient => 0x80,
            Modifier::Constructor => 0,
        };
    }
    if constructor {
        access |= 0x10000;
    }
    access
}

/// Flatten a `SmaliMethod` into a [`MethodInput`].
pub fn decode_method(
    class: &SmaliClass,
    method: &SmaliMethod,
    obfus: bool,
    dynamic_register: bool,
) -> Result<MethodInput, String> {
    let class_name = class.name.as_jni_type();
    let proto = method.signature.to_jni();
    let params = util::get_params_type(&proto);
    let mut ins_size: u32 = if method.modifiers.contains(&Modifier::Static) {
        0
    } else {
        1
    };
    for p in &params {
        ins_size += util::get_type_size(p) as u32;
    }
    let locals_declared = method.locals;
    let access = modifiers_to_access(&method.modifiers, method.constructor);
    let is_static = method.modifiers.contains(&Modifier::Static);
    // `.registers N` is the total register file; `.locals N` is everything below
    // the incoming arguments (androguard's `registers_size` / `ins_size`).
    let registers_size = match method.registers {
        Some(r) if r >= ins_size => r,
        Some(_) => {
            return Err(format!(
                "method {class_name}->{}{} declares .registers smaller than its arguments",
                method.name, proto
            ));
        }
        None => locals_declared + ins_size,
    };
    let locals = registers_size - ins_size;

    let mut ops: Vec<DecodedOp> = Vec::new();
    let mut labels: BTreeMap<String, LocIdx> = BTreeMap::new();
    // Payload directives with their target labels still unresolved.
    let mut raw_payloads: Vec<(String, RawPayload)> = Vec::new();
    let mut last_label: Option<String> = None;
    // `.catch` directives reference labels textually, and the *handler* label
    // normally comes after the directive, so they are resolved in a second pass.
    let mut catch_directives: Vec<CatchDirective> = Vec::new();

    enum RawPayload {
        Array(ArrayData),
        Packed {
            first_key: i32,
            targets: Vec<String>,
        },
        Sparse(Vec<(i32, String)>),
    }

    for op in &method.ops {
        match op {
            SmaliOp::Label(label) => {
                let name = strip_colon(&label.0);
                // A label marks the position of the next instruction.
                labels.insert(name.clone(), ops.len() as LocIdx);
                last_label = Some(name);
            }
            SmaliOp::Op(dexop) => {
                let (mnemonic, operands) = match dexop {
                    DexOp::Unused { .. } => (String::new(), String::new()),
                    other => {
                        let text = other.to_string();
                        match text.split_once(char::is_whitespace) {
                            Some((name, rest)) => (name.to_string(), rest.trim().to_string()),
                            None => (text, String::new()),
                        }
                    }
                };
                ops.push(DecodedOp {
                    op: dexop.clone(),
                    mnemonic,
                    operands,
                });
            }
            SmaliOp::ArrayData(dir) => {
                if let Some(label) = &last_label {
                    raw_payloads.push((label.clone(), RawPayload::Array(array_data_bytes(dir))));
                }
            }
            SmaliOp::PackedSwitch(dir) => {
                if let Some(label) = &last_label {
                    raw_payloads.push((
                        label.clone(),
                        RawPayload::Packed {
                            first_key: dir.first_key,
                            targets: dir.targets.iter().map(|l| strip_colon(&l.0)).collect(),
                        },
                    ));
                }
            }
            SmaliOp::SparseSwitch(dir) => {
                if let Some(label) = &last_label {
                    raw_payloads.push((
                        label.clone(),
                        RawPayload::Sparse(
                            dir.entries
                                .iter()
                                .map(|e| (e.key, strip_colon(&e.target.0)))
                                .collect(),
                        ),
                    ));
                }
            }
            SmaliOp::Catch(directive) => catch_directives.push(directive.clone()),
            // Debug directives carry no instructions.
            SmaliOp::Line(_)
            | SmaliOp::Prologue
            | SmaliOp::Epilogue
            | SmaliOp::Local { .. }
            | SmaliOp::EndLocal { .. }
            | SmaliOp::RestartLocal { .. } => {}
        }
    }

    // Resolve `.catch` directives now that every label is known.
    let mut catches: Vec<(LocIdx, LocIdx, String, LocIdx)> = Vec::new();
    for directive in &catch_directives {
        let (try_range, exception, handler) = match directive {
            CatchDirective::Catch {
                exception,
                try_range,
                handler,
            } => (try_range, exception.clone(), handler),
            CatchDirective::CatchAll { try_range, handler } => {
                (try_range, "Ljava/lang/Throwable;".to_string(), handler)
            }
        };
        let start = labels
            .get(&strip_colon(&try_range.start.0))
            .copied()
            .ok_or_else(|| {
                format!(
                    "unknown try-start label {} in {}.{}{}",
                    try_range.start, class_name, method.name, proto
                )
            })?;
        let end = labels
            .get(&strip_colon(&try_range.end.0))
            .copied()
            .ok_or_else(|| {
                format!(
                    "unknown try-end label {} in {}.{}{}",
                    try_range.end, class_name, method.name, proto
                )
            })?;
        let handler_idx = labels
            .get(&strip_colon(&handler.0))
            .copied()
            .ok_or_else(|| {
                format!(
                    "unknown handler label {} in {}.{}{}",
                    handler, class_name, method.name, proto
                )
            })?;
        catches.push((start, end, exception, handler_idx));
    }

    // Resolve payload target labels now that every label is known.
    let mut payloads: BTreeMap<String, Payload> = BTreeMap::new();
    for (label, raw) in raw_payloads {
        let payload = match raw {
            RawPayload::Array(data) => Payload::Array(data),
            RawPayload::Packed { first_key, targets } => {
                let mut resolved = Vec::with_capacity(targets.len());
                for t in targets {
                    resolved.push(*labels.get(&t).ok_or_else(|| {
                        format!("unknown switch target label :{t} in {class_name}->{proto}")
                    })?);
                }
                Payload::Packed {
                    first_key,
                    targets: resolved,
                }
            }
            RawPayload::Sparse(entries) => {
                let mut resolved = Vec::with_capacity(entries.len());
                for (key, t) in entries {
                    resolved.push((
                        key,
                        *labels.get(&t).ok_or_else(|| {
                            format!("unknown switch target label :{t} in {class_name}->{proto}")
                        })?,
                    ));
                }
                Payload::Sparse(resolved)
            }
        };
        payloads.insert(label, payload);
    }

    // Group handlers that share a try range into one range (the smali crate emits
    // one `.catch` per handler, typed first, catch-all last).
    let mut exceptions: Vec<ExceptionRange> = Vec::new();
    for (start, end, exception, handler) in catches {
        match exceptions
            .iter_mut()
            .find(|r| r.start == start && r.end == end)
        {
            Some(range) => range.handlers.push((exception, handler)),
            None => exceptions.push(ExceptionRange {
                start,
                end,
                handlers: vec![(exception, handler)],
            }),
        }
    }

    Ok(MethodInput {
        class_name,
        name: method.name.clone(),
        proto,
        access,
        is_static,
        locals,
        ins_size,
        registers_size,
        ops,
        labels,
        payloads,
        exceptions,
        obfus,
        dynamic_register,
        // The C++ writer emitted `LOGD` lines for every instruction; they are
        // opt-in here because they dwarf the generated code on real apps.
        trace: false,
    })
}
