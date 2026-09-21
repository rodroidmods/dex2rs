//! SSA intermediate representation — port of `dex2c/instruction.py`.
//!
//! Python builds a mutable object graph (instructions pointing at values, values
//! pointing back at their users). To keep that shape in Rust without
//! `Rc<RefCell<...>>` cycles, everything lives in one arena ([`Ir`]) addressed by
//! small copyable ids: [`ValId`], [`InsId`], [`BlockId`].
//!
//! Instruction *locations* ([`LocIdx`]) are indices into a method's linear
//! instruction list. Python used byte offsets in the dex; every use of an offset
//! there is either a branch target, a switch target or "the next instruction", so
//! indices are equivalent and cannot drift when instructions are widened.

use std::collections::BTreeMap;
use std::collections::HashSet;

use crate::util;

/// Location of an instruction inside the method's linear instruction list.
pub type LocIdx = i64;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ValId(pub u32);

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct InsId(pub u32);

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct BlockId(pub u32);

/// Binary arithmetic operators (`Op` in opcode_ins.py).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum BinOp {
    Cmp,
    Cmpl,
    Cmpg,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    ModF,
    ModD,
    And,
    Or,
    Xor,
    InShl,
    InShr,
    InUShr,
    LongShl,
    LongShr,
    LongUShr,
}

impl BinOp {
    /// The token the original writer emitted for this operator.
    pub fn c_op(self) -> &'static str {
        use BinOp::*;
        match self {
            Cmp => "cmp",
            Cmpl => "cmpl",
            Cmpg => "cmpg",
            Add => "+",
            Sub => "-",
            Mul => "*",
            Div => "/",
            Mod => "%",
            ModF => "fmodf",
            ModD => "fmod",
            And => "&",
            Or => "|",
            Xor => "^",
            InShl => "shl-int",
            InShr => "shr-int",
            InUShr => "ushr-int",
            LongShl => "shl-long",
            LongShr => "shr-long",
            LongUShr => "ushl-long",
        }
    }

    /// `div`/`rem` need the divide-by-zero guard (`write_div_expression`).
    pub fn needs_div_guard(self) -> bool {
        matches!(self, BinOp::Div | BinOp::Mod)
    }
}

/// Comparison operators used by `if-*` instructions.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CondOp {
    Equal,
    NEqual,
    Greater,
    Lower,
    GEqual,
    LEqual,
}

impl CondOp {
    pub fn c_op(self) -> &'static str {
        match self {
            CondOp::Equal => "==",
            CondOp::NEqual => "!=",
            CondOp::Greater => ">",
            CondOp::Lower => "<",
            CondOp::GEqual => ">=",
            CondOp::LEqual => "<=",
        }
    }

    /// `CONDS` from instruction.py, used when a comparison is folded into a branch.
    pub fn negate(self) -> CondOp {
        match self {
            CondOp::Equal => CondOp::NEqual,
            CondOp::NEqual => CondOp::Equal,
            CondOp::Lower => CondOp::GEqual,
            CondOp::LEqual => CondOp::Greater,
            CondOp::GEqual => CondOp::Lower,
            CondOp::Greater => CondOp::LEqual,
        }
    }
}

/// Unary operators (`neg` / `not`).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum UnOp {
    Neg,
    Not,
}

impl UnOp {
    /// The Rust spelling: bitwise complement is `!`, not C's `~`.
    pub fn c_op(self) -> &'static str {
        match self {
            UnOp::Neg => "-",
            UnOp::Not => "!",
        }
    }
}

/// A constant's payload: integers (also used for raw float/double bit patterns)
/// and strings.
#[derive(Clone, Debug, PartialEq)]
pub enum ConstVal {
    Int(i64),
    Str(String),
}

/// A user of a value: an instruction operand slot, or a phi input.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum User {
    Ins(InsId),
    Phi(ValId),
}

/// What kind of SSA value this is (`Variable`, `Phi` or `Constant`).
#[derive(Clone, Debug)]
pub enum ValKind {
    Const {
        value: ConstVal,
    },
    Var {
        register: i64,
        version: u32,
    },
    Phi {
        register: i64,
        version: u32,
        operands: BTreeMap<BlockId, ValId>,
        block: Option<BlockId>,
    },
}

#[derive(Clone, Debug)]
pub struct Value {
    pub kind: ValKind,
    /// Type descriptor ('I', 'J', 'Ljava/lang/String;', '[I', ...).
    pub ty: Option<String>,
    pub type_sealed: bool,
    /// Set once a `check-cast` has established [`Self::ty`]; see
    /// [`Self::cast_type`].
    pub cast_sealed: bool,
    pub definition: Option<InsId>,
    pub uses: Vec<User>,
    pub is_const: bool,
}

impl Value {
    pub fn register(&self) -> i64 {
        match &self.kind {
            ValKind::Const { .. } => -1,
            ValKind::Var { register, .. } | ValKind::Phi { register, .. } => *register,
        }
    }

    /// A short, readable description of a value, for diagnostics: register,
    /// SSA version and inferred type (`r2 v7 (J)`), or the constant kind.
    pub fn describe(&self) -> String {
        let ty = self.ty.as_deref().unwrap_or("?");
        match &self.kind {
            ValKind::Const { .. } => format!("const ({ty})"),
            ValKind::Var { register, version } => format!("r{register} v{version} ({ty})"),
            ValKind::Phi { register, .. } => format!("phi r{register} ({ty})"),
        }
    }

    pub fn version(&self) -> u32 {
        match &self.kind {
            ValKind::Const { .. } => 0,
            ValKind::Var { version, .. } | ValKind::Phi { version, .. } => *version,
        }
    }

    pub fn is_phi(&self) -> bool {
        matches!(self.kind, ValKind::Phi { .. })
    }

    pub fn is_const(&self) -> bool {
        matches!(self.kind, ValKind::Const { .. })
    }

    pub fn get_type(&self) -> Option<&str> {
        self.ty.as_deref()
    }

    /// `Value.set_type()` — seals the type: the first call wins unless the type
    /// is forced in again with the same value.
    pub fn set_type(&mut self, vtype: Option<&str>) -> bool {
        let Some(vtype) = vtype else {
            return false;
        };
        if self.type_sealed {
            return false;
        }
        self.type_sealed = true;
        match &self.ty {
            None => {
                self.ty = Some(vtype.to_string());
                true
            }
            Some(existing) if existing == vtype => false,
            Some(_) => {
                self.ty = Some(vtype.to_string());
                true
            }
        }
    }

    /// Overwrite the type of an SSA *artefact* -- a phi -- and seal it.
    ///
    /// A phi has no storage and emits no code of its own; its type exists only
    /// to guide the values that read it. Unlike a register it can therefore be
    /// corrected when a later inference iteration merges in a value of a
    /// different *kind*, which `refine_type` must refuse for a register (turning
    /// a `long` into an object would corrupt the code emitted for it).
    ///
    /// Sealing caps the correction at one change, so the inference fixpoint
    /// still terminates. Only ever called on a phi, and only when the kind
    /// changes; a same-kind update goes through `refine_type` as usual, which is
    /// what keeps a phi widening `I -> J` while its operands resolve.
    pub fn force_type(&mut self, vtype: &str) -> bool {
        self.type_sealed = true;
        if self.ty.as_deref() == Some(vtype) {
            return false;
        }
        self.ty = Some(vtype.to_string());
        true
    }

    /// Record the type *proved* by a `check-cast`, even over an already-sealed
    /// value. The first cast wins and seals; later casts are no-ops.
    ///
    /// `set_type` is "first write wins" and that is right for the inference
    /// rules, which only ever *deduce* a type -- but it cannot express a fact
    /// proved later in the method, which is exactly what `check-cast` is. In
    /// `iget-object v6, ..., field:Object; check-cast v6, [I` the `iget` seals
    /// `v6` as `Ljava/lang/Object;`, so the cast could not recover the array
    /// type; every `aget`/`aput` through `v6` then had no element type to infer
    /// and the method failed with "unknown type" (6 methods of the reference
    /// app).
    ///
    /// It must be *first*-cast-wins rather than a plain overwrite: one value can
    /// be cast more than once (the same register reused on different paths,
    /// reaching two `check-cast`s that disagree), and an overwrite makes the
    /// type-inference fixpoint oscillate between them until it gives up with
    /// "type infer failed" -- which is what a plain overwrite did to 5 other
    /// methods.
    pub fn cast_type(&mut self, vtype: Option<&str>) -> bool {
        let Some(vtype) = vtype else {
            return false;
        };
        if self.cast_sealed {
            return false;
        }
        self.cast_sealed = true;
        if self.ty.as_deref() == Some(vtype) {
            return false;
        }
        self.ty = Some(vtype.to_string());
        true
    }

    /// `Value.refine_type()` — keep the most specific type seen so far.
    pub fn refine_type(&mut self, vtype: Option<&str>) -> Result<bool, String> {
        let Some(vtype) = vtype else {
            return Ok(false);
        };
        if self.type_sealed || self.ty.as_deref() == Some(vtype) {
            return Ok(false);
        }
        let Some(current) = self.ty.clone() else {
            self.ty = Some(vtype.to_string());
            return Ok(true);
        };

        // Deliberate divergence from the reference, pinned by
        // `refine_type_keeps_a_narrower_reference`.
        //
        // `instruction.py` only has the second test, so refining a value whose
        // type is already `Ljava/lang/String;` with `Ljava/lang/Object;` falls
        // through to `merge_reference_type`, whose answer is `Object`: the value
        // is *widened*, and any later call through it has to go back through
        // reflection-style resolution. Keeping the narrower type is both more
        // precise and more faithful to what the register actually holds. The
        // effect is one extra method compiles on the reference app
        // (`androidx/room/e0;->b`), where the reference gives up with
        // "unable to refine type I Lf5/c;".
        if util::is_java_lang_object(vtype) && util::is_ref(&current) {
            return Ok(false);
        }
        if util::is_java_lang_object(&current) && util::is_ref(vtype) {
            return Ok(false);
        }

        if (util::is_int(&current) && util::is_int(vtype))
            || (util::is_float(&current) && util::is_float(vtype))
        {
            let new_type = util::get_smaller_type(&current, vtype);
            if new_type != current {
                self.ty = Some(new_type);
                Ok(true)
            } else {
                Ok(false)
            }
        } else if util::is_int(&current) && (util::is_float(vtype) || util::is_ref(vtype)) {
            // A constant promoted to float, or the reference-ish 0 -> NULL case.
            if self.is_const {
                self.ty = Some(vtype.to_string());
                Ok(true)
            } else if util::is_float(vtype) {
                // Lossless widening to a float type: keep the int type.
                Ok(false)
            } else {
                Err(format!("unable to refine type {current} {vtype}"))
            }
        } else {
            match util::merge_type(Some(&current), Some(vtype)) {
                None => Err(format!("unable to refine type {current} {vtype}")),
                Some(new_type) => {
                    if current != new_type {
                        self.ty = Some(new_type);
                        Ok(true)
                    } else {
                        Ok(false)
                    }
                }
            }
        }
    }

    pub fn use_empty(&self) -> bool {
        self.uses.is_empty()
    }
}

/// `Param` / `ThisParam`.
#[derive(Clone, Debug)]
pub struct Param {
    pub value: ValId,
    pub this: bool,
    pub declared: bool,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum InvokeType {
    Virtual,
    Super,
    Direct,
    Static,
    Interface,
}

impl InvokeType {
    pub fn as_str(self) -> &'static str {
        match self {
            InvokeType::Virtual => "virtual",
            InvokeType::Super => "super",
            InvokeType::Direct => "direct",
            InvokeType::Static => "static",
            InvokeType::Interface => "interface",
        }
    }

    pub fn is_static(self) -> bool {
        matches!(self, InvokeType::Static)
    }
}

/// Payload of a `fill-array-data` instruction, kept as the raw little-endian
/// bytes the dex stores so the destination array type decides the interpretation
/// (this is what the C++ writer did with `value.get_data()`).
#[derive(Clone, Debug)]
pub struct ArrayData {
    pub element_width: i32,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct InvokeData {
    pub invoke_type: InvokeType,
    pub clsdesc: String,
    pub name: String,
    pub thiz: Option<ValId>,
    pub rtype: String,
    pub ptype: Vec<String>,
    pub args: Vec<ValId>,
    pub result: Option<ValId>,
    pub triple: (String, String, String),
}

/// Which flavour of polymorphic target an `invoke-polymorphic` names.
///
/// The generated code emits the *runtime symbol* for this
/// (`d2c::HANDLE_METHOD` / `d2c::HANDLE_VAR`), so the two crates cannot drift
/// apart over a constant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolymorphicKind {
    MethodHandle,
    VarHandle,
}

impl PolymorphicKind {
    pub fn runtime_const(self) -> &'static str {
        match self {
            PolymorphicKind::MethodHandle => "d2c::HANDLE_METHOD",
            PolymorphicKind::VarHandle => "d2c::HANDLE_VAR",
        }
    }

    /// The receiver type, used to refine the target register.
    pub fn receiver_type(self) -> &'static str {
        match self {
            PolymorphicKind::MethodHandle => "Ljava/lang/invoke/MethodHandle;",
            PolymorphicKind::VarHandle => "Ljava/lang/invoke/VarHandle;",
        }
    }
}

/// `invoke-polymorphic` / `invoke-polymorphic/range`.
///
/// The instruction calls a *signature-polymorphic* method: `MethodHandle.invoke`,
/// `invokeExact`, or a `VarHandle` access mode. There is no method to resolve and
/// no `jvalue` call to emit — the JVM dispatches these on the caller's descriptor,
/// which JNI cannot name — so the whole call is one runtime helper
/// (`d2c::invoke_polymorphic`) that drives `MethodHandle.invokeWithArguments`.
#[derive(Clone, Debug)]
pub struct PolymorphicInvokeData {
    /// `MethodHandle` or `VarHandle` receiver (register 0).
    pub target: ValId,
    pub kind: PolymorphicKind,
    /// The method as the dex names it: `invokeExact`, or the access mode
    /// (`getAndAdd`), which `AccessMode.valueFromMethodName` maps for us.
    pub name: String,
    /// Call-site parameters, from the proto operand (`([III)I` -> `[I`, `I`, `I`).
    pub ptype: Vec<String>,
    pub rtype: String,
    pub args: Vec<ValId>,
    pub result: Option<ValId>,
}

/// `const-method-type` / `const-method-handle` — a `java.lang.invoke` constant.
///
/// `MethodType` and `MethodHandle` have no JNI equivalent, so both are one call
/// into the runtime (`d2c::method_type` / `d2c::method_handle`), which builds
/// them through `MethodType.methodType` and `MethodHandles.Lookup`. The handle's
/// `kind` is the DEX `method_handle_item.handle_type`; for a field handle the
/// descriptor is the *field* type and for a method handle it is the method
/// prototype.
#[derive(Clone, Debug)]
pub enum ConstInvokeValue {
    MethodType {
        proto: String,
    },
    MethodHandle {
        kind: u8,
        clsdesc: String,
        name: String,
        desc: String,
    },
}

/// Trace information for the `LOGD` line the C++ writer emitted per instruction.
#[derive(Clone, Debug, Default)]
pub struct DvmInfo {
    pub name: String,
    pub output: String,
}

/// Instruction kinds — one variant per class in `dex2c/instruction.py`.
#[derive(Clone, Debug)]
pub enum InsKind {
    LoadConstant {
        value: ValId,
        cst: ValId,
        obfus: bool,
    },
    MoveParam {
        param: Param,
    },
    /// `MoveExpression`
    Move {
        lhs: ValId,
        rhs: ValId,
    },
    /// `MoveResultExpression`
    MoveResult {
        lhs: ValId,
        rhs: ValId,
    },
    /// `ArrayStoreInstruction`
    ArrayStore {
        rhs: ValId,
        array: ValId,
        index: ValId,
        elem_type: Option<String>,
    },
    /// `StaticInstruction`
    StaticPut {
        rhs: ValId,
        clsdesc: String,
        ftype: String,
        name: String,
    },
    /// `InstanceInstruction`
    InstancePut {
        lhs: ValId,
        rhs: ValId,
        clsdesc: String,
        atype: String,
        name: String,
    },
    /// `NewInstance`
    NewInstance {
        value: ValId,
        ins_type: String,
    },
    /// `InvokeInstruction` (+ range variants)
    Invoke(InvokeData),
    /// `invoke-polymorphic` (+ range)
    InvokePolymorphic(PolymorphicInvokeData),
    /// `const-method-type` / `const-method-handle`
    ConstInvoke {
        value: ValId,
        cst: ConstInvokeValue,
    },
    /// `ReturnInstruction` (`rtype` is `None` for `return-void`)
    Return {
        rtype: Option<String>,
        arg: Option<ValId>,
    },
    /// `NopExpression`
    Nop,
    /// `GotoInst`
    Goto {
        target: LocIdx,
    },
    /// `SwitchExpression`
    Switch {
        src: ValId,
        cases: Vec<(i32, LocIdx)>,
    },
    /// `CheckCastExpression`
    CheckCast {
        arg: ValId,
        clsdesc: String,
    },
    /// `InstanceOfExpression`
    InstanceOf {
        value: ValId,
        obj: ValId,
        clsdesc: String,
    },
    /// `ArrayLoadExpression`
    ArrayLoad {
        value: ValId,
        array: ValId,
        index: ValId,
        elem_type: Option<String>,
    },
    /// `ArrayLengthExpression`
    ArrayLength {
        value: ValId,
        array: ValId,
    },
    /// `NewArrayExpression`
    NewArray {
        value: ValId,
        size: ValId,
        atype: String,
    },
    /// `FilledArrayExpression`
    FilledArray {
        value: ValId,
        size: u32,
        atype: String,
        args: Vec<ValId>,
    },
    /// `FillArrayExpression`
    FillArray {
        reg: ValId,
        data: ArrayData,
    },
    /// `MoveExceptionExpression`
    MoveException {
        value: ValId,
        ty: String,
    },
    /// `MonitorEnterExpression`
    MonitorEnter {
        r: ValId,
    },
    /// `MonitorExitExpression`
    MonitorExit {
        r: ValId,
    },
    /// `ThrowExpression`
    Throw {
        r: ValId,
    },
    /// `BinaryExpression` / `BinaryCompExpression`
    Binary {
        op: BinOp,
        value: ValId,
        arg1: ValId,
        arg2: ValId,
        op_type: String,
        /// True for `BinaryCompExpression` (`cmp*`): the result is an int.
        comp: bool,
    },
    /// `UnaryExpression`
    Unary {
        value: ValId,
        op: UnOp,
        arg: ValId,
        ty: String,
    },
    /// `CastExpression`
    Cast {
        value: ValId,
        op: String,
        dest_type: String,
        arg: ValId,
        src_type: String,
    },
    /// `ConditionalExpression`
    Cond {
        op: CondOp,
        arg1: ValId,
        arg2: ValId,
        target: LocIdx,
    },
    /// `ConditionalZExpression`
    CondZ {
        op: CondOp,
        arg: ValId,
        target: LocIdx,
    },
    /// `InstanceExpression`
    GetInstance {
        value: ValId,
        arg: ValId,
        ftype: String,
        clsdesc: String,
        name: String,
    },
    /// `StaticExpression`
    GetStatic {
        value: ValId,
        ftype: String,
        clsdesc: String,
        name: String,
    },
}

impl InsKind {
    /// `Instruction.set_value()` — only instructions that define a value can be
    /// retargeted (the Python base class allowed it for every instruction).
    pub fn set_value(&mut self, v: ValId) {
        use InsKind::*;
        match self {
            LoadConstant { value, .. }
            | Move { lhs: value, .. }
            | MoveResult { lhs: value, .. }
            | NewInstance { value, .. }
            | InstanceOf { value, .. }
            | ArrayLoad { value, .. }
            | ArrayLength { value, .. }
            | NewArray { value, .. }
            | FilledArray { value, .. }
            | MoveException { value, .. }
            | Binary { value, .. }
            | Unary { value, .. }
            | Cast { value, .. }
            | GetInstance { value, .. }
            | GetStatic { value, .. } => *value = v,
            Invoke(data) => data.result = Some(v),
            InvokePolymorphic(data) => data.result = Some(v),
            ConstInvoke { value, .. } => *value = v,
            _ => {}
        }
    }

    /// `Instruction.get_value()`
    pub fn value(&self) -> Option<ValId> {
        use InsKind::*;
        match self {
            LoadConstant { value, .. }
            | Move { lhs: value, .. }
            | MoveResult { lhs: value, .. }
            | NewInstance { value, .. }
            | InstanceOf { value, .. }
            | ArrayLoad { value, .. }
            | ArrayLength { value, .. }
            | NewArray { value, .. }
            | FilledArray { value, .. }
            | MoveException { value, .. }
            | Binary { value, .. }
            | Unary { value, .. }
            | Cast { value, .. }
            | GetInstance { value, .. }
            | GetStatic { value, .. } => Some(*value),
            Invoke(data) => data.result,
            InvokePolymorphic(data) => data.result,
            ConstInvoke { value, .. } => Some(*value),
            _ => None,
        }
    }

    /// Every operand slot, mirroring the Python `operands` list.
    pub fn for_each_operand(&self, mut f: impl FnMut(ValId)) {
        use InsKind::*;
        match self {
            LoadConstant { cst, .. } => f(*cst),
            MoveParam { .. } => {}
            Move { rhs, .. } | MoveResult { rhs, .. } => f(*rhs),
            ArrayStore {
                rhs, array, index, ..
            } => {
                f(*rhs);
                f(*array);
                f(*index);
            }
            StaticPut { rhs, .. } => f(*rhs),
            InstancePut { lhs, rhs, .. } => {
                f(*lhs);
                f(*rhs);
            }
            NewInstance { .. } => {}
            Invoke(data) => {
                if let Some(thiz) = data.thiz {
                    f(thiz);
                }
                for a in &data.args {
                    f(*a);
                }
            }
            InvokePolymorphic(data) => {
                f(data.target);
                for a in &data.args {
                    f(*a);
                }
            }
            ConstInvoke { .. } => {}
            Return { arg, .. } => {
                if let Some(a) = arg {
                    f(*a);
                }
            }
            Nop | Goto { .. } => {}
            Switch { src, .. } => f(*src),
            CheckCast { arg, .. } => f(*arg),
            InstanceOf { obj, .. } => f(*obj),
            ArrayLoad { array, index, .. } => {
                f(*array);
                f(*index);
            }
            ArrayLength { array, .. } => f(*array),
            NewArray { size, .. } => f(*size),
            FilledArray { args, .. } => {
                for a in args {
                    f(*a);
                }
            }
            FillArray { reg, .. } => f(*reg),
            MoveException { .. } => {}
            MonitorEnter { r } | MonitorExit { r } | Throw { r } => f(*r),
            Binary { arg1, arg2, .. } => {
                f(*arg1);
                f(*arg2);
            }
            Unary { arg, .. } => f(*arg),
            Cast { arg, .. } => f(*arg),
            Cond { arg1, arg2, .. } => {
                f(*arg1);
                f(*arg2);
            }
            CondZ { arg, .. } => f(*arg),
            GetInstance { arg, .. } => f(*arg),
            GetStatic { .. } => {}
        }
    }

    /// Replace every operand equal to `old`; returns true when something changed.
    pub fn replace_operand(&mut self, old: ValId, new: ValId) -> bool {
        let mut changed = false;
        let mut slot = |v: &mut ValId| {
            if *v == old {
                *v = new;
                changed = true;
            }
        };
        use InsKind::*;
        match self {
            LoadConstant { cst, .. } => slot(cst),
            MoveParam { param } => {
                if param.value == old {
                    param.value = new;
                    changed = true;
                }
            }
            Move { rhs, .. } | MoveResult { rhs, .. } => slot(rhs),
            ArrayStore {
                rhs, array, index, ..
            } => {
                slot(rhs);
                slot(array);
                slot(index);
            }
            StaticPut { rhs, .. } => slot(rhs),
            InstancePut { lhs, rhs, .. } => {
                slot(lhs);
                slot(rhs);
            }
            NewInstance { .. } => {}
            Invoke(data) => {
                if let Some(thiz) = data.thiz.as_mut() {
                    slot(thiz);
                }
                for a in data.args.iter_mut() {
                    slot(a);
                }
            }
            InvokePolymorphic(data) => {
                slot(&mut data.target);
                for a in data.args.iter_mut() {
                    slot(a);
                }
            }
            ConstInvoke { .. } => {}
            Return { arg, .. } => {
                if let Some(a) = arg.as_mut() {
                    slot(a);
                }
            }
            Nop | Goto { .. } => {}
            Switch { src, .. } => slot(src),
            CheckCast { arg, .. } => slot(arg),
            InstanceOf { obj, .. } => slot(obj),
            ArrayLoad { array, index, .. } => {
                slot(array);
                slot(index);
            }
            ArrayLength { array, .. } => slot(array),
            NewArray { size, .. } => slot(size),
            FilledArray { args, .. } => {
                for a in args.iter_mut() {
                    slot(a);
                }
            }
            FillArray { reg, .. } => slot(reg),
            MoveException { .. } => {}
            MonitorEnter { r } | MonitorExit { r } | Throw { r } => slot(r),
            Binary { arg1, arg2, .. } => {
                slot(arg1);
                slot(arg2);
            }
            Unary { arg, .. } => slot(arg),
            Cast { arg, .. } => slot(arg),
            Cond { arg1, arg2, .. } => {
                slot(arg1);
                slot(arg2);
            }
            CondZ { arg, .. } => slot(arg),
            GetInstance { arg, .. } => slot(arg),
            GetStatic { .. } => {}
        }
        changed
    }

    /// `Instruction.get_class()` — the class descriptor to cache for this instruction.
    pub fn get_class(&self) -> Option<String> {
        use InsKind::*;
        match self {
            StaticPut { clsdesc, .. }
            | InstancePut { clsdesc, .. }
            | Invoke(InvokeData { clsdesc, .. })
            | CheckCast { clsdesc, .. }
            | InstanceOf { clsdesc, .. }
            | GetInstance { clsdesc, .. }
            | GetStatic { clsdesc, .. } => Some(clsdesc.clone()),
            NewInstance { ins_type, .. } => Some(ins_type.clone()),
            NewArray { atype, .. } | FilledArray { atype, .. } => Some(atype[1..].to_string()),
            _ => None,
        }
    }

    /// `Instruction.get_field()` — the field cache key.
    pub fn get_field(&self) -> Option<String> {
        use InsKind::*;
        match self {
            StaticPut { clsdesc, name, .. }
            | InstancePut { clsdesc, name, .. }
            | GetInstance { clsdesc, name, .. }
            | GetStatic { clsdesc, name, .. } => Some(format!("{clsdesc}.{name}")),
            _ => None,
        }
    }

    /// `Instruction.get_call_method()` — the method cache key.
    pub fn get_call_method(&self) -> Option<String> {
        use InsKind::*;
        match self {
            Invoke(data) => Some(format!(
                "{}->{}({})",
                data.clsdesc,
                data.name,
                data.ptype.join("")
            )),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct InsNode {
    pub kind: InsKind,
    pub parent: Option<BlockId>,
    /// `Instruction.offset` — the instruction's own location.
    pub offset: LocIdx,
    /// `Instruction.next_offset` — the following instruction's location.
    pub next_offset: LocIdx,
    pub dvm: Option<DvmInfo>,
}

impl InsNode {
    pub fn value(&self) -> Option<ValId> {
        self.kind.value()
    }
}

/// `IrBasicBlock` — with `landing` set when this node is a landing pad.
#[derive(Clone, Debug)]
pub struct Block {
    /// Location of the block's first instruction (`-1` for landing pads).
    pub start: LocIdx,
    /// Exclusive end of the block's instruction span (the next block's start).
    pub end: LocIdx,
    pub ins: Vec<InsId>,
    pub move_params: Vec<InsId>,
    pub var_to_declare: Vec<ValId>,
    pub class_to_declare: Vec<String>,
    pub field_to_declare: Vec<String>,
    pub method_to_declare: Vec<String>,
    /// `IrBasicBlock.num` — the `LN` label number.
    pub num: i64,
    /// Post-order index used to derive `num` (`compute_rpo`).
    pub po: i64,
    pub in_catch: bool,
    pub catch_type: Option<String>,
    pub filled: bool,
    pub sealed: bool,
    pub current_definitions: BTreeMap<i64, ValId>,
    /// Register -> value as of the block's last instruction that can throw.
    ///
    /// A handler reached from this block sees the registers *at that
    /// instruction*, not at the block's end: an instruction that cannot throw
    /// cannot transfer control, so a register it overwrites never reaches the
    /// handler. Copying the block-end state instead is what leaked a `long`
    /// into a `monitor-exit`'s handler when a register was reused for both
    /// (`FileChannelDataSource;->copyTo`). Only recorded for blocks covered by
    /// an exception range; `None` means "no throwing instruction".
    pub catch_values: Option<BTreeMap<i64, ValId>>,
    pub incomplete_phis: Vec<ValId>,
    pub phis: Vec<ValId>,
    pub catch_successors: Vec<BlockId>,
}

impl Block {
    pub fn new(start: LocIdx) -> Self {
        Block {
            start,
            end: start,
            ins: Vec::new(),
            move_params: Vec::new(),
            var_to_declare: Vec::new(),
            class_to_declare: Vec::new(),
            field_to_declare: Vec::new(),
            method_to_declare: Vec::new(),
            num: -1,
            po: 0,
            in_catch: false,
            catch_type: None,
            filled: false,
            sealed: false,
            current_definitions: BTreeMap::new(),
            catch_values: None,
            incomplete_phis: Vec::new(),
            phis: Vec::new(),
            catch_successors: Vec::new(),
        }
    }

    /// `IrBasicBlock.label`
    pub fn label(&self) -> String {
        format!("BB_{:x}", self.start)
    }
}

/// `LandingPad` — one exception dispatch target shared by a range of blocks.
#[derive(Clone, Debug)]
pub struct LandingPad {
    /// The block whose `num` names this pad (`EX_LandingPad_N`).
    pub source: BlockId,
    /// Catch type descriptor -> handler block, in the order the dex lists them
    /// (typed handlers first, catch-all last).
    pub handles: Vec<(String, BlockId)>,
}

impl LandingPad {
    pub fn label(&self, blocks: &[Block]) -> String {
        format!("EX_LandingPad_{}", blocks[self.source.0 as usize].num)
    }
}

/// The whole method: values, instructions, blocks, CFG edges and method metadata.
#[derive(Default, Debug)]
pub struct Ir {
    pub values: Vec<Value>,
    pub insns: Vec<InsNode>,
    pub blocks: Vec<Block>,
    pub edges: Vec<Vec<BlockId>>,
    pub catch_edges: Vec<Vec<BlockId>>,
    pub reverse_edges: Vec<Vec<BlockId>>,
    pub reverse_catch_edges: Vec<Vec<BlockId>>,
    pub landing_pads: Vec<LandingPad>,
    pub node_to_landing_pad: BTreeMap<BlockId, usize>,
    pub rpo: Vec<BlockId>,
    pub entry: Option<BlockId>,
    pub offset_to_node: BTreeMap<LocIdx, BlockId>,

    // ---- method metadata (from the class/method being compiled) ----
    pub class_name: String,
    pub name: String,
    pub proto: String,
    pub rtype: String,
    pub params: Vec<i64>,
    pub params_type: Vec<String>,
    pub access: Vec<String>,
    pub is_static: bool,
    pub dynamic_register: bool,
    pub obfus: bool,
    /// Emit the per-instruction `d2c::trace(...)` calls (opt-in: they account
    /// for a large share of the generated source and the original only logged
    /// in debug builds).
    pub trace: bool,
    pub registers_size: i64,
    pub ins_size: i64,
    pub var_versions: BTreeMap<i64, u32>,
}

impl Ir {
    pub fn new_value(&mut self, kind: ValKind) -> ValId {
        let is_const = matches!(kind, ValKind::Const { .. });
        self.values.push(Value {
            kind,
            ty: None,
            type_sealed: false,
            cast_sealed: false,
            definition: None,
            uses: Vec::new(),
            is_const,
        });
        ValId((self.values.len() - 1) as u32)
    }

    pub fn new_const(&mut self, value: ConstVal, ty: Option<&str>) -> Result<ValId, String> {
        let id = self.new_value(ValKind::Const { value });
        if let Some(t) = ty {
            self.values[id.0 as usize].refine_type(Some(t))?;
        }
        Ok(id)
    }

    pub fn new_var(&mut self, register: i64, version: u32) -> ValId {
        self.new_value(ValKind::Var { register, version })
    }

    pub fn new_phi(&mut self, register: i64, version: u32) -> ValId {
        self.new_value(ValKind::Phi {
            register,
            version,
            operands: BTreeMap::new(),
            block: None,
        })
    }

    #[inline]
    pub fn value(&self, id: ValId) -> &Value {
        &self.values[id.0 as usize]
    }

    #[inline]
    pub fn value_mut(&mut self, id: ValId) -> &mut Value {
        &mut self.values[id.0 as usize]
    }

    #[inline]
    pub fn ins(&self, id: InsId) -> &InsNode {
        &self.insns[id.0 as usize]
    }

    #[inline]
    pub fn ins_mut(&mut self, id: InsId) -> &mut InsNode {
        &mut self.insns[id.0 as usize]
    }

    #[inline]
    pub fn block(&self, id: BlockId) -> &Block {
        &self.blocks[id.0 as usize]
    }

    #[inline]
    pub fn block_mut(&mut self, id: BlockId) -> &mut Block {
        &mut self.blocks[id.0 as usize]
    }

    /// Push an instruction and register a user edge for each of its operands,
    /// mirroring the `value.add_user(self)` calls in the Python constructors.
    pub fn add_ins(&mut self, kind: InsKind) -> InsId {
        self.insns.push(InsNode {
            kind,
            parent: None,
            offset: -1,
            next_offset: -1,
            dvm: None,
        });
        let id = InsId((self.insns.len() - 1) as u32);
        let mut operands = Vec::new();
        self.insns[id.0 as usize]
            .kind
            .for_each_operand(|v| operands.push(v));
        for v in operands {
            self.add_user(v, User::Ins(id));
        }
        id
    }

    pub fn add_user(&mut self, val: ValId, user: User) {
        let uses = &mut self.values[val.0 as usize].uses;
        if !uses.contains(&user) {
            uses.push(user);
        }
    }

    pub fn remove_user(&mut self, val: ValId, user: User) {
        self.values[val.0 as usize].uses.retain(|u| *u != user);
    }

    /// `Value.replace_all_uses_with()`
    pub fn replace_all_uses_with(&mut self, old: ValId, new: ValId) {
        let users = self.values[old.0 as usize].uses.clone();
        for user in users {
            match user {
                User::Ins(ins) => {
                    let changed = self.ins_mut(ins).kind.replace_operand(old, new);
                    if changed {
                        self.add_user(new, User::Ins(ins));
                        self.remove_user(old, User::Ins(ins));
                    }
                }
                User::Phi(phi) => {
                    let mut changed = false;
                    if let ValKind::Phi { operands, .. } = &mut self.value_mut(phi).kind {
                        for v in operands.values_mut() {
                            if *v == old {
                                *v = new;
                                changed = true;
                            }
                        }
                    }
                    if changed {
                        self.add_user(new, User::Phi(phi));
                        self.remove_user(old, User::Phi(phi));
                    }
                }
            }
        }
        self.value_mut(old).uses.clear();
    }

    /// `Instruction.replase_use_of_with()`
    pub fn replace_use_of_with(&mut self, ins: InsId, old: ValId, new: ValId) {
        if self.ins_mut(ins).kind.replace_operand(old, new) {
            self.add_user(new, User::Ins(ins));
            self.remove_user(old, User::Ins(ins));
        }
    }

    /// `Phi.replase_use_of_with()`
    pub fn phi_replace_use_of_with(&mut self, phi: ValId, old: ValId, new: ValId) {
        let mut changed = false;
        if let ValKind::Phi { operands, .. } = &mut self.value_mut(phi).kind {
            for v in operands.values_mut() {
                if *v == old {
                    *v = new;
                    changed = true;
                }
            }
        }
        if changed {
            self.add_user(new, User::Phi(phi));
            self.remove_user(old, User::Phi(phi));
        }
    }

    pub fn phi_operands(&self, phi: ValId) -> Option<&BTreeMap<BlockId, ValId>> {
        match &self.value(phi).kind {
            ValKind::Phi { operands, .. } => Some(operands),
            _ => None,
        }
    }

    /// `Phi.set_block()`
    pub fn set_phi_block(&mut self, phi: ValId, block: BlockId) {
        if let ValKind::Phi { block: b, .. } = &mut self.value_mut(phi).kind {
            *b = Some(block);
        }
    }

    /// `Phi.get_operands().get(pred)`
    pub fn phi_operand(&self, phi: ValId, pred: BlockId) -> Option<ValId> {
        self.phi_operands(phi).and_then(|m| m.get(&pred).copied())
    }

    pub fn phi_add_operand(&mut self, phi: ValId, pred: BlockId, value: ValId) {
        if let ValKind::Phi { operands, .. } = &mut self.value_mut(phi).kind {
            operands.insert(pred, value);
        }
        self.add_user(value, User::Phi(phi));
    }

    /// `Value.get_users()` for an instruction's defined value.
    pub fn users_of_value(&self, val: ValId) -> Vec<User> {
        self.values[val.0 as usize].uses.clone()
    }

    /// `Instruction.get_users()` — the users of the value this instruction defines.
    pub fn users_of_ins(&self, ins: InsId) -> Vec<User> {
        match self.ins(ins).value() {
            Some(v) => self.users_of_value(v),
            None => Vec::new(),
        }
    }

    /// `Value.get_uses()` count, used by the writer's dead-local-ref check.
    pub fn use_count(&self, val: ValId) -> usize {
        self.values[val.0 as usize].uses.len()
    }

    /// Renumber every block by code position; the generated labels use `num`.
    pub fn compute_block_order(&mut self) -> Vec<BlockId> {
        crate::graph::compute_block_order(self)
    }

    /// `Phi.remove_trivial_phi()` (returns true when the phi was removed).
    pub fn phi_remove_trivial_phi(&mut self, phi: ValId) -> bool {
        let operands: Vec<ValId> = match &self.value(phi).kind {
            ValKind::Phi { operands, .. } => operands.values().copied().collect(),
            _ => return false,
        };
        let mut same: Option<ValId> = None;
        for op in &operands {
            if Some(*op) == same || *op == phi {
                continue;
            }
            if same.is_some() {
                return false;
            }
            same = Some(*op);
        }
        let Some(same) = same else {
            return false;
        };

        for op in &operands {
            self.remove_user(*op, User::Phi(phi));
        }
        self.replace_all_uses_with(phi, same);
        if let ValKind::Phi { block, .. } = &self.value(phi).kind {
            let block = *block;
            if let Some(block) = block {
                self.block_mut(block).phis.retain(|p| *p != phi);
            }
        }
        true
    }

    /// `Phi.resolve_type()`.
    pub fn phi_resolve_type(&mut self, phi: ValId) -> Result<bool, String> {
        let operands: Vec<ValId> = match &self.value(phi).kind {
            ValKind::Phi { operands, .. } => operands.values().copied().collect(),
            _ => return Ok(false),
        };

        let mut same_op_type: Option<String> = None;
        for op in &operands {
            if self.value(*op).is_const() {
                continue;
            }
            let op_type = self.value(*op).get_type().map(|s| s.to_string());
            if let Some(current) = &same_op_type {
                if op_type.as_deref() != Some(current.as_str()) {
                    same_op_type = util::merge_type(Some(current), op_type.as_deref());
                    continue;
                }
            }
            same_op_type = op_type;
        }

        let new_type = match same_op_type {
            Some(t) => Some(t),
            None => self.value(phi).get_type().map(|s| s.to_string()),
        };

        let mut changed = false;
        if let Some(new_type) = new_type {
            let phi_type = self.value(phi).get_type().map(|s| s.to_string());
            if phi_type.as_deref() != Some(new_type.as_str()) {
                // An early iteration can give the phi a scalar type while only
                // the scalar operand has been typed yet; a later one then merges
                // in a reference and `refine_type` correctly refuses to convert
                // a `long` into an object, abandoning the method. The phi itself
                // *is* usable as that reference (it is the register's value at a
                // point where the verifier knows it is an object), and being an
                // SSA artefact it can be re-typed. See `Value::force_type`.
                match phi_type.as_deref() {
                    Some(t) if util::is_scalar(t) != util::is_scalar(&new_type) => {
                        changed |= self.value_mut(phi).force_type(&new_type);
                    }
                    _ => changed |= self.value_mut(phi).refine_type(Some(&new_type))?,
                }
            }
            for op in &operands {
                if self.value(*op).is_const() {
                    continue;
                }
                if self.value(*op).get_type() == Some(new_type.as_str()) {
                    continue;
                }
                // A phi whose incoming values disagree in *kind* -- one a
                // primitive, one a reference -- is a register reused for two
                // unrelated purposes on different paths. The DEX verifier
                // permits it (it types per program point), a single SSA value
                // cannot express it, and `refine_type` correctly refuses to turn
                // a `long` into an object. The reference instead asserts out of
                // `merge_reference_type` and loses the whole method
                // (`apksig/.../FileChannelDataSource;->copyTo`). Keep each
                // operand's own type: only the phi itself, which is what every
                // *use* of the register consults, takes the merged one.
                let op_type = self.value(*op).get_type().map(|s| s.to_string());
                if let Some(op_type) = op_type {
                    if util::is_scalar(&op_type) != util::is_scalar(&new_type) {
                        continue;
                    }
                }
                changed |= self.value_mut(*op).refine_type(Some(&new_type))?;
            }
        }
        Ok(changed)
    }

    /// `Instruction.dump()` — the LOGD trace text (empty when there is no dvm info).
    pub fn dump(&self, ins: InsId) -> String {
        let node = self.ins(ins);
        match &node.dvm {
            Some(d) if !d.name.is_empty() => format!(
                "{:x}:{} {}",
                node.offset,
                d.name,
                util::hex_escape_string(&d.output)
            ),
            _ => String::new(),
        }
    }

    // ---------------------------------------------------------------- CFG ----

    pub fn sucs(&self, node: BlockId) -> Vec<BlockId> {
        self.edges.get(node.0 as usize).cloned().unwrap_or_default()
    }

    pub fn all_sucs(&self, node: BlockId) -> Vec<BlockId> {
        let mut out = self.sucs(node);
        out.extend(
            self.catch_edges
                .get(node.0 as usize)
                .cloned()
                .unwrap_or_default(),
        );
        out
    }

    pub fn all_catches(&self, node: BlockId) -> Vec<BlockId> {
        self.catch_edges
            .get(node.0 as usize)
            .cloned()
            .unwrap_or_default()
    }

    pub fn preds(&self, node: BlockId) -> Vec<BlockId> {
        self.reverse_edges
            .get(node.0 as usize)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|n| !self.block(*n).in_catch)
            .collect()
    }

    pub fn all_preds(&self, node: BlockId) -> Vec<BlockId> {
        let mut out = self
            .reverse_edges
            .get(node.0 as usize)
            .cloned()
            .unwrap_or_default();
        out.extend(
            self.reverse_catch_edges
                .get(node.0 as usize)
                .cloned()
                .unwrap_or_default(),
        );
        out
    }

    pub fn add_edge(&mut self, e1: BlockId, e2: BlockId) {
        if !self.edges[e1.0 as usize].contains(&e2) {
            self.edges[e1.0 as usize].push(e2);
        }
        if !self.reverse_edges[e2.0 as usize].contains(&e1) {
            self.reverse_edges[e2.0 as usize].push(e1);
        }
    }

    pub fn remove_edge(&mut self, e1: BlockId, e2: BlockId) {
        self.edges[e1.0 as usize].retain(|n| *n != e2);
        self.reverse_edges[e2.0 as usize].retain(|n| *n != e1);
    }

    pub fn add_catch_edge(&mut self, e1: BlockId, e2: BlockId) {
        if !self.catch_edges[e1.0 as usize].contains(&e2) {
            self.catch_edges[e1.0 as usize].push(e2);
        }
        if !self.reverse_catch_edges[e2.0 as usize].contains(&e1) {
            self.reverse_catch_edges[e2.0 as usize].push(e1);
        }
    }

    // ------------------------------------------------------ type inference ---

    /// A short, readable name for an instruction, for diagnostics.
    ///
    /// The type-inference errors (`unable to refine type`, `unknown type`)
    /// otherwise name only the *values* involved, which makes a failure report
    /// from a device nearly impossible to act on without rebuilding.
    pub fn ins_summary(&self, id: InsId) -> String {
        let s = format!("{:?}", self.ins(id).kind);
        match s.find(['(', ' ']) {
            Some(i) => s[..i].to_string(),
            None => s,
        }
    }

    /// `Instruction.resolve_type()` dispatch.
    ///
    /// The instruction is cloned out of the arena first: the inference rules read
    /// and write values, so holding a `&mut InsNode` across them is not possible.
    pub fn resolve_type(&mut self, id: InsId) -> Result<bool, String> {
        let kind = self.ins(id).kind.clone();
        use InsKind::*;
        match kind {
            LoadConstant { .. } | Nop | Goto { .. } | FillArray { .. } => {
                // `LoadConstant.resolve_type` returns False; the constant already
                // refined the value's type when it was built.
                Ok(false)
            }
            Move { lhs, rhs, .. } | MoveResult { lhs, rhs, .. } => {
                let op_type = self.value(rhs).get_type().map(|s| s.to_string());
                let value_type = self.value(lhs).get_type().map(|s| s.to_string());
                let Some(new_type) = util::merge_type(op_type.as_deref(), value_type.as_deref())
                else {
                    return Ok(false);
                };
                let mut changed = false;
                if self.value(rhs).type_sealed {
                    changed |= self.value_mut(lhs).set_type(Some(&new_type));
                } else {
                    changed |= self.value_mut(lhs).refine_type(Some(&new_type))?;
                    changed |= self.value_mut(rhs).refine_type(Some(&new_type))?;
                }
                Ok(changed)
            }
            ArrayStore {
                rhs,
                array,
                index,
                elem_type,
            } => {
                let elem = self.array_elem_type(elem_type.as_deref(), array, rhs);
                let mut changed = self.value_mut(index).set_type(Some("I"));
                if let Some(elem) = elem {
                    changed |= self.value_mut(rhs).refine_type(Some(&elem))?;
                    changed |= self
                        .value_mut(array)
                        .refine_type(Some(&format!("[{elem}")))?;
                }
                Ok(changed)
            }
            StaticPut { rhs, ftype, .. } => self.value_mut(rhs).refine_type(Some(&ftype)),
            InstancePut {
                lhs,
                rhs,
                clsdesc,
                atype,
                ..
            } => {
                let mut changed = self.value_mut(lhs).refine_type(Some(&clsdesc))?;
                changed |= self.value_mut(rhs).refine_type(Some(&atype))?;
                Ok(changed)
            }
            NewInstance { value, ins_type } => Ok(self.value_mut(value).set_type(Some(&ins_type))),
            Invoke(data) => {
                let mut changed = false;
                if let Some(thiz) = data.thiz {
                    let thiz_ty = self.value(thiz).get_type().map(|s| s.to_string());
                    if thiz_ty.as_deref() != Some(data.clsdesc.as_str()) {
                        changed |= self.value_mut(thiz).refine_type(Some(&data.clsdesc))?;
                    }
                }
                for (idx, arg) in data.args.iter().enumerate() {
                    if let Some(ptype) = data.ptype.get(idx) {
                        changed |= self.value_mut(*arg).refine_type(Some(ptype))?;
                    }
                }
                if let Some(result) = data.result {
                    changed |= self.value_mut(result).set_type(Some(&data.rtype));
                }
                Ok(changed)
            }
            // `invoke-polymorphic` types exactly like an ordinary call: the
            // receiver is a `MethodHandle`/`VarHandle`, each argument takes the
            // parameter type from the call-site prototype, and an *unboxed*
            // result takes the prototype's return type -- which is why the
            // prototype, not the erased JNI descriptor, is what the value is
            // refined with.
            InvokePolymorphic(data) => {
                let receiver = data.kind.receiver_type();
                let mut changed = self.value_mut(data.target).refine_type(Some(receiver))?;
                for (idx, arg) in data.args.iter().enumerate() {
                    if let Some(ptype) = data.ptype.get(idx) {
                        changed |= self.value_mut(*arg).refine_type(Some(ptype))?;
                    }
                }
                if let Some(result) = data.result {
                    changed |= self.value_mut(result).set_type(Some(&data.rtype));
                }
                Ok(changed)
            }
            // Both constants are a fresh object of a known class.
            ConstInvoke { value, cst } => {
                let ty = match cst {
                    ConstInvokeValue::MethodType { .. } => "Ljava/lang/invoke/MethodType;",
                    ConstInvokeValue::MethodHandle { .. } => "Ljava/lang/invoke/MethodHandle;",
                };
                Ok(self.value_mut(value).set_type(Some(ty)))
            }
            Return { rtype, arg } => match (rtype, arg) {
                (Some(rtype), Some(arg)) => self.value_mut(arg).refine_type(Some(&rtype)),
                _ => Ok(false),
            },
            Switch { src, .. } => self.value_mut(src).refine_type(Some("I")),
            // `check-cast` is a proof, not a hint: after it the register *is* of
            // the cast type, or a `ClassCastException` has already been thrown.
            //
            // The shared refine rule refuses to narrow `java.lang.Object` to a
            // reference (`refine_type`'s Object guard, which the reference
            // implements too), so an array recovered by a cast -- `(int[]) obj`,
            // which is how a field or a call returning `Object` becomes an array
            // again -- keeps the type `Object`. Every `aget`/`aput` through it
            // then has no element type to infer, the destination value never gets
            // a type, and the method fails with "unknown type" instead of
            // converting. That path is why 4 methods of the reference app could
            // not be converted.
            //
            // Forcing the type for an *array* target is both sound (the cast
            // guarantees it) and necessary (the element type is only recoverable
            // from the array descriptor). Non-array targets keep the reference's
            // behaviour, where the loss is harmless: a call resolves its method
            // from the instruction's own class reference, not from the local's
            // type.
            CheckCast { arg, clsdesc } => {
                if !util::is_array(&clsdesc) {
                    return self.value_mut(arg).refine_type(Some(&clsdesc));
                }
                // A value read by casts to two *different* array types has no
                // single type: the same register holds a `String[]` on one path
                // and a `long[]` on another (`t4/h0;->f` is a switch over the
                // array kind). Both types are live, so picking either one is
                // wrong -- and picking the first one seen silently changed that
                // method's `aget-wide` from `GetLongArrayRegion` to an
                // object-element read. Leave the inferred type alone in that
                // case; uses still narrow it (`Long.valueOf(J)` is what pins the
                // element type there), which is exactly how the reference got it
                // right before this rule existed.
                let conflict = self.value(arg).uses.iter().any(|u| match u {
                    User::Ins(i) => match &self.ins(*i).kind {
                        CheckCast { clsdesc: other, .. } => other != &clsdesc,
                        _ => false,
                    },
                    User::Phi(_) => false,
                });
                if conflict {
                    return Ok(false);
                }
                // `cast_type`, not `set_type`: the value was very likely sealed
                // to `Ljava/lang/Object;` by the `iget`/`invoke` that produced
                // it, and only a cast-specific write recovers the element type.
                // See `Value::cast_type`.
                Ok(self.value_mut(arg).cast_type(Some(&clsdesc)))
            }
            InstanceOf {
                value,
                obj,
                clsdesc,
            } => {
                let mut changed = self.value_mut(obj).refine_type(Some(&clsdesc))?;
                changed |= self.value_mut(value).set_type(Some("Z"));
                Ok(changed)
            }
            ArrayLoad {
                value,
                array,
                index,
                elem_type,
            } => {
                let elem = self.array_elem_type(elem_type.as_deref(), array, value);
                let mut changed = false;
                if let Some(elem) = elem {
                    changed |= self.value_mut(value).refine_type(Some(&elem))?;
                    changed |= self
                        .value_mut(array)
                        .refine_type(Some(&format!("[{elem}")))?;
                }
                changed |= self.value_mut(index).refine_type(Some("I"))?;
                Ok(changed)
            }
            ArrayLength { value, .. } => Ok(self.value_mut(value).set_type(Some("I"))),
            NewArray { value, size, atype } => {
                let mut changed = self.value_mut(value).set_type(Some(&atype));
                changed |= self.value_mut(size).set_type(Some("I"));
                Ok(changed)
            }
            FilledArray {
                value, atype, args, ..
            } => {
                let mut changed = self.value_mut(value).set_type(Some(&atype));
                let elem = atype[1..].to_string();
                for arg in args {
                    changed |= self.value_mut(arg).refine_type(Some(&elem))?;
                }
                Ok(changed)
            }
            MoveException { value, ty } => {
                // `move-exception` gets the catch type of its block; a catch-all
                // handler has none, and `refine_type(None)` was a no-op in Python.
                if ty.is_empty() {
                    Ok(false)
                } else {
                    self.value_mut(value).refine_type(Some(&ty))
                }
            }
            MonitorEnter { r } | MonitorExit { r } => self
                .value_mut(r)
                .refine_type(Some("Ljava/lang/Object;"))
                .map_err(|e| {
                    let users: Vec<String> = self
                        .value(r)
                        .uses
                        .iter()
                        .map(|u| match u {
                            User::Ins(i) => self.ins_summary(*i),
                            User::Phi(_) => "phi".to_string(),
                        })
                        .collect();
                    format!(
                        "{e} (monitor operand {}, used by [{}])",
                        self.value(r).describe(),
                        users.join(", ")
                    )
                }),
            Throw { r } => Ok(self.value_mut(r).set_type(Some("Ljava/lang/Throwable;"))),
            Binary {
                value,
                arg1,
                arg2,
                op_type,
                comp,
                ..
            } => {
                let mut changed = if comp {
                    self.value_mut(value).refine_type(Some("I"))?
                } else {
                    self.value_mut(value).refine_type(Some(&op_type))?
                };
                changed |= self.value_mut(arg1).refine_type(Some(&op_type))?;
                changed |= self.value_mut(arg2).refine_type(Some(&op_type))?;
                Ok(changed)
            }
            Unary { value, ty, .. } => Ok(self.value_mut(value).set_type(Some(&ty))),
            Cast {
                value,
                dest_type,
                arg,
                src_type,
                ..
            } => {
                let mut changed = self.value_mut(value).refine_type(Some(&dest_type))?;
                changed |= self.value_mut(arg).refine_type(Some(&src_type))?;
                Ok(changed)
            }
            Cond { arg1, arg2, .. } => {
                let type1 = self.value(arg1).get_type().map(|s| s.to_string());
                let type2 = self.value(arg2).get_type().map(|s| s.to_string());
                let is_ref1 = type1
                    .as_deref()
                    .map(|t| util::is_ref(t) || util::is_array(t))
                    .unwrap_or(false);
                let is_ref2 = type2
                    .as_deref()
                    .map(|t| util::is_ref(t) || util::is_array(t))
                    .unwrap_or(false);
                if is_ref1 && is_ref2 {
                    return Ok(false);
                }
                let Some(new_type) = util::merge_type(type1.as_deref(), type2.as_deref()) else {
                    return Ok(false);
                };
                let mut changed = self.value_mut(arg1).refine_type(Some(&new_type))?;
                changed |= self.value_mut(arg2).refine_type(Some(&new_type))?;
                Ok(changed)
            }
            // `ConditionalZExpression.resolve_type` cannot infer anything.
            CondZ { .. } => Ok(false),
            GetInstance {
                value,
                arg,
                ftype,
                clsdesc,
                ..
            } => {
                let mut changed = self.value_mut(value).set_type(Some(&ftype));
                changed |= self.value_mut(arg).refine_type(Some(&clsdesc))?;
                Ok(changed)
            }
            GetStatic { value, ftype, .. } => Ok(self.value_mut(value).set_type(Some(&ftype))),
            MoveParam { .. } => Ok(false),
        }
    }

    /// `ArrayStoreInstruction.get_elem_type()` / `ArrayLoadExpression.get_elem_type()`
    pub fn array_elem_type_for(
        &self,
        explicit: Option<&str>,
        array: ValId,
        other: ValId,
    ) -> Option<String> {
        self.array_elem_type(explicit, array, other)
    }

    fn array_elem_type(
        &self,
        explicit: Option<&str>,
        array: ValId,
        other: ValId,
    ) -> Option<String> {
        if let Some(e) = explicit {
            if !e.is_empty() {
                return Some(e.to_string());
            }
        }
        let type1 = self
            .value(array)
            .get_type()
            .filter(|t| util::is_array(t))
            .map(|t| t[1..].to_string());
        let type2 = self.value(other).get_type().map(|s| s.to_string());
        util::merge_type(type1.as_deref(), type2.as_deref())
    }

    /// `Constant.get_constant()`: object-typed constants are hex-escaped (the
    /// `AY_OBFUSCATE` macro reads a hex literal), the rest render as plain text.
    pub fn constant_get(&self, val: ValId) -> String {
        let is_ref = self
            .value(val)
            .get_type()
            .map(|t| t.starts_with('L'))
            .unwrap_or(false);
        match &self.value(val).kind {
            ValKind::Const {
                value: ConstVal::Str(s),
            } => {
                if is_ref {
                    util::hex_escape_string(s)
                } else {
                    s.clone()
                }
            }
            ValKind::Const {
                value: ConstVal::Int(i),
            } => i.to_string(),
            _ => String::new(),
        }
    }

    /// `Constant.__str__` — how a constant is spliced into generated expressions.
    pub fn constant_literal(&self, val: ValId) -> String {
        let atype = self.value(val).get_type().map(|s| s.to_string());
        match (&self.value(val).kind, atype.as_deref()) {
            (
                ValKind::Const {
                    value: ConstVal::Int(i),
                },
                Some("F"),
            ) => {
                format!("d2c_bitcast_to_float({i})")
            }
            (
                ValKind::Const {
                    value: ConstVal::Int(i),
                },
                Some("D"),
            ) => {
                format!("d2c_bitcast_to_double({i})")
            }
            (
                ValKind::Const {
                    value: ConstVal::Int(i),
                },
                Some(t),
            ) if util::is_int(t) || util::is_long(t) => i.to_string(),
            _ => self.constant_get(val),
        }
    }

    /// `Constant.__str__` for a constant (alias kept for readability at call sites).
    pub fn constant_str(&self, val: ValId) -> String {
        self.constant_literal(val)
    }

    /// `Instruction.get_class()` including `LoadConstant`'s `Class` special case.
    pub fn ins_get_class(&self, ins: InsId) -> Option<String> {
        if let InsKind::LoadConstant { cst, .. } = &self.ins(ins).kind {
            let cst_type = self.value(*cst).get_type().map(|s| s.to_string());
            if cst_type.as_deref() == Some("Ljava/lang/Class;") {
                return Some(self.constant_get(*cst));
            }
            return None;
        }
        self.ins(ins).kind.get_class()
    }

    /// `Instruction.get_field()`
    pub fn ins_get_field(&self, ins: InsId) -> Option<String> {
        self.ins(ins).kind.get_field()
    }

    /// `Instruction.get_call_method()`
    pub fn ins_get_call_method(&self, ins: InsId) -> Option<String> {
        self.ins(ins).kind.get_call_method()
    }

    /// The raw string behind a string constant (used when emitting literals).
    pub fn constant_string(&self, val: ValId) -> Option<String> {
        match &self.value(val).kind {
            ValKind::Const {
                value: ConstVal::Str(s),
            } => Some(s.clone()),
            _ => None,
        }
    }

    pub fn constant_int(&self, val: ValId) -> Option<i64> {
        match &self.value(val).kind {
            ValKind::Const {
                value: ConstVal::Int(i),
            } => Some(*i),
            _ => None,
        }
    }

    /// `Value.__str__` for trace purposes.
    pub fn value_name(&self, val: ValId) -> String {
        match &self.value(val).kind {
            ValKind::Const { .. } => self.constant_literal(val),
            ValKind::Var { register, version } => {
                if *register >= 0 {
                    format!("v{register}_{version}")
                } else {
                    format!("vResult_{version}")
                }
            }
            ValKind::Phi {
                register, version, ..
            } => {
                if *register >= 0 {
                    format!("v{register}_{version}")
                } else {
                    format!("vResult_{version}")
                }
            }
        }
    }

    /// Registers that currently hold a definition (mirrors `read_current_definition`
    /// bookkeeping when a block has to be walked).
    pub fn defined_registers(&self, block: BlockId) -> HashSet<i64> {
        self.block(block)
            .current_definitions
            .keys()
            .copied()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var(ty: &str) -> Value {
        Value {
            kind: ValKind::Var {
                register: 0,
                version: 1,
            },
            ty: Some(ty.to_string()),
            type_sealed: false,
            cast_sealed: false,
            definition: None,
            uses: Vec::new(),
            is_const: false,
        }
    }

    /// `check-cast` proves a type, so it must be able to set one over a value
    /// that an earlier inference rule already sealed (`iget-object` seals
    /// `Ljava/lang/Object;`). It must also be *first*-cast-wins: an overwrite
    /// makes the inference fixpoint oscillate when one register is cast twice.
    #[test]
    fn cast_type_recovers_an_array_over_a_sealed_object() {
        let mut value = var("Ljava/lang/Object;");
        // `set_type` reports `false` when the type does not change, but it still
        // seals -- which is the state that makes the cast necessary.
        value.set_type(Some("Ljava/lang/Object;"));
        assert!(value.type_sealed);

        // `set_type`/`refine_type` cannot do this: both respect the seal and
        // `refine_type` additionally refuses to narrow `Object` to a reference.
        assert_eq!(value.refine_type(Some("[I")), Ok(false));
        assert!(!value.set_type(Some("[I")));
        assert_eq!(value.ty.as_deref(), Some("Ljava/lang/Object;"));

        assert!(value.cast_type(Some("[I")));
        assert_eq!(value.ty.as_deref(), Some("[I"));
        // A second, different cast must not overwrite: that is what made
        // `copyTo`-style methods oscillate to "type infer failed".
        assert!(!value.cast_type(Some("[Ljava/lang/String;")));
        assert_eq!(value.ty.as_deref(), Some("[I"));
        assert!(!value.cast_type(Some("[I")));
    }

    /// A phi is an SSA artefact with no storage, so a later iteration may merge
    /// in a value of a different *kind* and the phi has to be able to take it.
    /// `refine_type` refuses that (turning a `long` into an object would corrupt
    /// the code emitted for a register), and the refusal is what lost
    /// `apksig/.../FileChannelDataSource;->copyTo`.
    #[test]
    fn force_type_corrects_a_phi_when_the_merged_kind_changes() {
        let mut phi = var("J");
        assert!(matches!(
            phi.refine_type(Some("Ljava/lang/Object;")),
            Err(_)
        ));
        assert!(phi.force_type("Ljava/lang/Object;"));
        assert_eq!(phi.ty.as_deref(), Some("Ljava/lang/Object;"));
        // Sealed, so the fixpoint terminates after the one correction.
        assert!(!phi.force_type("Ljava/lang/Object;"));
    }

    /// The predicate that decides whether a phi merge mixes a primitive with a
    /// reference, and so has to keep each operand's own type.
    #[test]
    fn is_scalar_separates_primitives_from_references() {
        for scalar in ["I", "J", "F", "D", "Z", "B", "C", "S"] {
            assert!(util::is_scalar(scalar), "{scalar} is a primitive");
        }
        for reference in [
            "Ljava/lang/Object;",
            "Ljava/lang/String;",
            "[I",
            "[J",
            "[Ljava/lang/Object;",
        ] {
            assert!(!util::is_scalar(reference), "{reference} is a reference");
        }
    }

    /// The one place the port knowingly differs from the reference.
    ///
    /// `instruction.py` widens a reference to `java.lang.Object` when that is
    /// what a later use claims, because it only guards the opposite direction
    /// (`var_type` is Object, incoming is a reference). Keeping the narrower
    /// type is intentional -- see the comment in `refine_type` -- so this test
    /// exists to notice if someone ever "fixes" it back to the reference's
    /// behaviour, and to record that the divergence is a decision rather than
    /// drift.
    #[test]
    fn refine_type_keeps_a_narrower_reference() {
        let mut value = var("Ljava/lang/String;");
        assert_eq!(value.refine_type(Some("Ljava/lang/Object;")), Ok(false));
        assert_eq!(value.ty.as_deref(), Some("Ljava/lang/String;"));

        // The direction the reference does guard: Object refined by a subtype
        // takes the subtype.
        let mut value = var("Ljava/lang/Object;");
        assert_eq!(value.refine_type(Some("Ljava/lang/String;")), Ok(false));
        assert_eq!(value.ty.as_deref(), Some("Ljava/lang/Object;"));

        // An unknown type takes whatever arrives.
        let mut value = Value {
            ty: None,
            ..var("I")
        };
        assert_eq!(value.refine_type(Some("Ljava/lang/String;")), Ok(true));
        assert_eq!(value.ty.as_deref(), Some("Ljava/lang/String;"));
    }

    /// A constant zero may become any reference (`0 -> NULL`), but a non-constant
    /// int refined to a reference is the case the reference calls
    /// "unable to refine type", and the port must still refuse it.
    #[test]
    fn refine_type_still_refuses_an_int_to_reference() {
        let mut value = var("I");
        let error = value
            .refine_type(Some("Lf5/c;"))
            .expect_err("an int is not a reference");
        assert!(error.contains("unable to refine type"), "{error}");
    }
}
