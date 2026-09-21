//! Rust source emitter — port of `dex2c/writer.py`.
//!
//! The Python tool emitted C++ for each compiled method; this emits the Rust
//! equivalent. The structure is deliberately unchanged:
//!
//! * one `extern "system"` function per method, named with the JNI mangled name;
//! * `jclass` / `jfieldID` / `jmethodID` locals that cache resolutions;
//! * every instruction wrapped in an exception-handling scope whose failure path
//!   jumps to the method's landing pad.
//!
//! C++'s `goto L%d` has no Rust equivalent, so each basic block becomes an arm of
//! a `match` inside a dispatcher loop and control flow is expressed by assigning
//! `__state` and `continue`ing. Landing pads get negative state numbers; the
//! "unwind block" is the `return` after the loop.
//!
//! Phi nodes (which the C++ writer could not express at all) are lowered by
//! copying the incoming operand at the end of each predecessor, right before the
//! jump.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::ir::{BinOp, CondOp, DvmInfo, InsId, InsKind, Ir, ValId};
use crate::ssa::{IrMethod, RegisterAllocator};
use crate::util;

/// Loop label used by every generated dispatcher.
const LOOP: &str = "'d2c";

/// How many times the dispatcher may re-enter the *same* state before it
/// reports itself as frozen (see the freeze guard in [`Writer::write_method`]).
const SPIN_LIMIT: u32 = 100_000;

pub struct Writer<'a> {
    method: &'a IrMethod,
    ir: &'a Ir,
    ra: &'a RegisterAllocator,
    buf: String,
    /// The `(args)ret` descriptor, filled in by [`Writer::write_method`].
    prototype: String,
    /// Class/field/method cache slot names, numbered from the entry block's
    /// declaration lists exactly like `TmpnameAllocator` was.
    names: TmpNames,
}

/// `TmpnameAllocator` from writer.py: stable `cls0`, `fld3`, `mth7` names.
///
/// writer.py builds one allocator per family, so `cls`, `fld` and `mth` each
/// count from zero independently (`cls0`/`fld0`/`mth0` in the same method).
#[derive(Default)]
struct TmpNames {
    numbering: BTreeMap<String, BTreeMap<String, usize>>,
}

impl TmpNames {
    fn get(&mut self, item: &str, prefix: &str) -> String {
        let family = self.numbering.entry(prefix.to_string()).or_default();
        let next = family.len();
        let index = *family.entry(item.to_string()).or_insert(next);
        format!("{prefix}{index}")
    }

    /// Like [`TmpNames::get`] but for keys that must already be allocated.
    fn existing(&self, item: &str, prefix: &str) -> Option<String> {
        self.numbering
            .get(prefix)?
            .get(item)
            .map(|index| format!("{prefix}{index}"))
    }
}

impl<'a> Writer<'a> {
    pub fn new(method: &'a IrMethod) -> Result<Self, String> {
        let mut names = TmpNames::default();
        if let Some(entry) = method.ir.entry {
            let block = method.ir.block(entry);
            for name in &block.class_to_declare {
                names.get(name, "cls");
            }
            for name in &block.field_to_declare {
                names.get(name, "fld");
            }
            for name in &block.method_to_declare {
                names.get(name, "mth");
            }
        }
        Ok(Writer {
            method,
            ir: &method.ir,
            ra: &method.ra,
            buf: String::new(),
            prototype: String::new(),
            names,
        })
    }

    /// The `(args)ret` JNI descriptor, available after [`Writer::write_method`].
    pub fn prototype(&mut self) -> Result<String, String> {
        if self.prototype.is_empty() {
            self.write_method()?;
        }
        Ok(self.prototype.clone())
    }

    fn writeln(&mut self, s: &str) {
        self.buf.push_str(s);
        self.buf.push('\n');
    }

    fn fmt(&mut self, args: std::fmt::Arguments<'_>) {
        let _ = self.buf.write_fmt(args);
    }

    // ------------------------------------------------------------ helpers ----

    fn ra(&self, val: ValId) -> Result<usize, String> {
        self.ra.get(self.ir, val)
    }

    fn var(&self, val: ValId) -> Result<String, String> {
        Ok(format!("v{}", self.ra(val)?))
    }

    fn is_const(&self, val: ValId) -> bool {
        matches!(self.ir.value(val).kind, crate::ir::ValKind::Const { .. })
    }

    /// `get_variable_or_const()` — constants are spliced in as literals.
    fn operand(&mut self, val: ValId) -> Result<String, String> {
        if self.is_const(val) {
            Ok(self.ir.constant_literal(val))
        } else {
            self.var(val)
        }
    }

    /// The Rust type used for a local of descriptor `ty` (`get_cdecl_type`).
    fn local_type(ty: &str) -> &'static str {
        match util::get_cdecl_type(ty) {
            "jlong" => "d2c::jni_sys::jlong",
            "jfloat" => "d2c::jni_sys::jfloat",
            "jdouble" => "d2c::jni_sys::jdouble",
            "jobject" => "d2c::jni_sys::jobject",
            _ => "d2c::jni_sys::jint",
        }
    }

    /// The JNI return type of the method (`get_native_type`), as Rust.
    fn return_type(ty: &str) -> &'static str {
        match ty {
            "V" => "()",
            // `d2c::jboolean` is the ABI-faithful byte, not `jni_sys`'s `bool`.
            "Z" => "d2c::jboolean",
            "B" => "d2c::jni_sys::jbyte",
            "S" => "d2c::jni_sys::jshort",
            "C" => "d2c::jni_sys::jchar",
            "I" => "d2c::jni_sys::jint",
            "J" => "d2c::jni_sys::jlong",
            "F" => "d2c::jni_sys::jfloat",
            "D" => "d2c::jni_sys::jdouble",
            _ => "d2c::jni_sys::jobject",
        }
    }

    /// `get_type_descriptor()` — the `<Type>` infix JNI uses (`Int`, `Object`, ...).
    fn descriptor(ty: &str) -> &'static str {
        util::type_descriptor(ty)
    }

    /// The `jv_*` constructor for a `jvalue` of descriptor `ty`.
    fn jvalue_ctor(ty: &str) -> &'static str {
        match ty {
            "Z" => "jv_boolean",
            "B" => "jv_byte",
            "C" => "jv_char",
            "S" => "jv_short",
            "I" => "jv_int",
            "J" => "jv_long",
            "F" => "jv_float",
            "D" => "jv_double",
            _ => "jv_object",
        }
    }

    /// Lower-case element name used by the runtime's whole-array helpers
    /// (`d2c::set_int_array`).
    fn elem_name(ty: &str) -> Option<&'static str> {
        Some(match ty {
            "Z" => "boolean",
            "B" => "byte",
            "C" => "char",
            "S" => "short",
            "I" => "int",
            "J" => "long",
            "F" => "float",
            "D" => "double",
            _ => return None,
        })
    }

    /// The `<Type>` infix of the runtime's one-to-one JNI wrappers
    /// (`d2c::GetStaticIntField`, `d2c::NewIntArray`, ...).
    fn jni_elem_name(ty: &str) -> Option<&'static str> {
        match ty {
            "Z" | "B" | "C" | "S" | "I" | "J" | "F" | "D" => Some(Self::descriptor(ty)),
            _ => None,
        }
    }

    /// Values narrower than `jint` need a widening cast into their slot, which is
    /// always `jint` (exactly as `get_cdecl_type` decided for the C++ writer).
    fn widen(ty: &str, expr: String) -> String {
        match ty {
            "Z" | "B" | "C" | "S" => format!("({expr}) as d2c::jni_sys::jint"),
            _ => expr,
        }
    }

    /// `write_define_ex_handle()` — where a failure inside this instruction goes.
    fn pad_of(&self, ins: InsId) -> Option<usize> {
        let parent = self.ir.ins(ins).parent?;
        self.ir.node_to_landing_pad.get(&parent).copied()
    }

    /// The `{ ... }` the original emitted for `goto EX_HANDLE`.
    fn ex_jump(&self, pad: Option<usize>) -> String {
        match pad {
            Some(pad) => format!("{{ __state = {}; continue {LOOP}; }}", pad_state(pad)),
            None => format!("{{ break {LOOP}; }}"),
        }
    }

    /// `EX_UnwindBlock: return ...;`
    fn unwind_return(&self) -> String {
        let rtype = self.method.rtype.as_str();
        if rtype.starts_with('V') {
            "return;".to_string()
        } else if util::is_ref(rtype) {
            "return core::ptr::null_mut();".to_string()
        } else {
            format!("return Default::default(); /* {} */", rtype)
        }
    }

    fn write_trace(&mut self, ins: InsId) {
        if !self.ir.trace {
            return;
        }
        let DvmInfo { name, output } = match self.ir.ins(ins).dvm.as_ref() {
            Some(d) if !d.name.is_empty() => d.clone(),
            _ => return,
        };
        let text = format!("{:x}:{} {}", self.ir.ins(ins).offset, name, output);
        let literal = bytes_literal(util::hex_escape_string(&text).as_bytes());
        self.fmt(format_args!("                d2c::trace({literal});\n"));
    }

    // ------------------------------------------------------- local refs ------

    /// `write_kill_local_reference()`
    ///
    /// Emitted whenever a reference-typed slot is about to be overwritten, so it is a
    /// `DeleteLocalRef` on whatever the slot currently holds -- and that is *not*
    /// necessarily a reference. Inference gives a register one type for the whole method,
    /// while dex lets a register hold a reference on one path and a small integer on
    /// another, so the slot can be holding the integer when the delete runs: the emitted
    /// text for one real case is
    ///
    /// ```text
    /// v6 = 2 as d2c::jni_sys::jobject;          // const/4 v6, #2 on this path
    /// ...
    /// if !v6.is_null() { d2c::delete_local_ref(env, v6); }   // 2 is read as kind 'global'
    /// ```
    ///
    /// and ART answers that with SIGABRT (see `jni::delete_local_ref`, which filters it).
    /// The emitter deliberately still writes the call: whether the slot holds a reference
    /// at that point is a question about *emission order* through a state machine that can
    /// enter a block from several predecessors, which is not something a per-instruction
    /// check can answer. The invariant is kept at the one place that can answer it -- the
    /// runtime, where the actual value is in hand.
    fn kill_local_ref(&mut self, val: ValId, tmp: bool) -> Result<(), String> {
        let ty = self.ir.value(val).get_type().unwrap_or("").to_string();
        if !util::is_ref(&ty) {
            return Ok(());
        }
        if !tmp && self.ir.value(val).register() < 0 {
            return Ok(());
        }
        let var = self.var(val)?;
        self.fmt(format_args!(
            "                if !{var}.is_null() {{ d2c::delete_local_ref(env, {var}); }}\n"
        ));
        Ok(())
    }

    /// `write_delete_dead_local_reference()`
    fn delete_dead_local_ref(&mut self, val: ValId) -> Result<(), String> {
        if self.ir.use_count(val) == 0 && self.ir.value(val).register() < 0 {
            self.kill_local_ref(val, true)?;
        }
        Ok(())
    }

    /// `visit_move()` / a phi copy: `dest = <src>` with reference bookkeeping.
    /// Spell out a numeric conversion between two host types.
    ///
    /// Rust has no implicit numeric conversions, so copying or comparing two
    /// different widths has to be written out where C++ converted silently:
    /// `v_int = v_long;` is a compile error here. Those pairs come from a
    /// register that inference gave two widths (a `long` on the path that wrote
    /// it, a 32-bit value at this point) -- legitimate in DEX, which types per
    /// program point. The reference emitted the bare assignment and relied on
    /// C++; the cast keeps the emitted behaviour identical while compiling.
    ///
    /// Only numerics are touched: a `jobject` on either side is left alone,
    /// because a pointer conversion is not something C++ accepted either.
    fn coerce_numeric(from: &'static str, to: &'static str, expr: String) -> String {
        if from == to {
            return expr;
        }
        let target = match to {
            "jint" | "jlong" | "jfloat" | "jdouble" => to,
            _ => return expr,
        };
        if !matches!(from, "jint" | "jlong" | "jfloat" | "jdouble") {
            return expr;
        }
        format!("({expr}) as d2c::jni_sys::{target}")
    }

    fn emit_move(&mut self, dest: ValId, src: ValId) -> Result<(), String> {
        // A move whose two ends share a slot is a no-op. Skipping it also avoids
        // the `DeleteLocalRef` + `NewLocalRef` pair on the same reference, which
        // could free the object being re-created.
        if !self.is_const(src) && self.ra(dest)? == self.ra(src)? {
            return Ok(());
        }
        self.kill_local_ref(dest, false)?;
        let dest_var = self.var(dest)?;
        let src_type = self.ir.value(src).get_type().unwrap_or("").to_string();
        let expr = if util::is_primitive_type(&src_type) {
            self.operand(src)?
        } else {
            let src_var = self.var(src)?;
            format!("d2c::new_local_ref(env, {src_var})")
        };
        // A move copies a register, and the two ends can carry different widths
        // when inference resolved them against different paths.
        let dest_type = self.ir.value(dest).get_type().unwrap_or("").to_string();
        let expr = Self::coerce_numeric(
            util::get_cdecl_type(&src_type),
            util::get_cdecl_type(&dest_type),
            expr,
        );
        self.fmt(format_args!("                {dest_var} = {expr};\n"));
        Ok(())
    }

    // ------------------------------------------------------- declarations ----

    fn write_declarations(&mut self) -> Result<(), String> {
        let entry = self
            .ir
            .entry
            .ok_or_else(|| "method has no entry block".to_string())?;
        let block = self.ir.block(entry);

        let mut declared = Vec::new();
        for var in &block.var_to_declare {
            let slot = self.ra(*var)?;
            if declared.contains(&slot) {
                continue;
            }
            declared.push(slot);
            let ty = self
                .ir
                .value(*var)
                .get_type()
                .ok_or_else(|| format!("unknown type for {}", self.ir.value_name(*var)))?
                .to_string();
            let local = Self::local_type(&ty);
            let init = if util::is_ref(&ty) {
                "core::ptr::null_mut()"
            } else {
                "Default::default()"
            };
            self.fmt(format_args!("        let mut v{slot}: {local} = {init};\n"));
        }

        if !block.var_to_declare.is_empty() && !self.ir.landing_pads.is_empty() {
            self.writeln(
                "        let mut exception: d2c::jni_sys::jthrowable = core::ptr::null_mut();",
            );
        }

        let mut classes = Vec::new();
        let mut fields = Vec::new();
        let mut methods = Vec::new();
        for name in &block.class_to_declare {
            if !classes.contains(name) {
                classes.push(name.clone());
            }
        }
        for name in &block.field_to_declare {
            if !fields.contains(name) {
                fields.push(name.clone());
            }
        }
        for name in &block.method_to_declare {
            if !methods.contains(name) {
                methods.push(name.clone());
            }
        }

        for name in &classes {
            let slot = self
                .names
                .existing(name, "cls")
                .ok_or_else(|| format!("class cache slot for {name} was never allocated"))?;
            self.fmt(format_args!(
                "        let mut {slot}: d2c::jni_sys::jclass = core::ptr::null_mut();\n"
            ));
        }
        for name in &fields {
            let slot = self
                .names
                .existing(name, "fld")
                .ok_or_else(|| format!("field cache slot for {name} was never allocated"))?;
            self.fmt(format_args!(
                "        let mut {slot}: d2c::jni_sys::jfieldID = core::ptr::null_mut();\n"
            ));
        }
        for name in &methods {
            let slot = self
                .names
                .existing(name, "mth")
                .ok_or_else(|| format!("method cache slot for {name} was never allocated"))?;
            self.fmt(format_args!(
                "        let mut {slot}: d2c::jni_sys::jmethodID = core::ptr::null_mut();\n"
            ));
        }
        Ok(())
    }

    // ------------------------------------------------------------ method -----

    pub fn write_method(&mut self) -> Result<String, String> {
        let entry = self
            .ir
            .entry
            .ok_or_else(|| "method has no entry block".to_string())?;
        let jni_name = util::jni_long_name(&self.ir.class_name, &self.ir.name, &self.ir.proto);
        self.prototype = self.ir.proto.clone();

        self.fmt(format_args!(
            "\n/* {}->{}{} */\n",
            self.ir.class_name, self.ir.name, self.ir.proto
        ));
        // The emitter keeps the C++ writer's parenthesisation, so the generated
        // expressions are noisier than a human would write them.
        self.writeln(
            "#[allow(unreachable_code, unused_variables, unused_assignments, unused_mut, \
             unused_unsafe, unused_parens, non_snake_case, clippy::all)]",
        );
        // Statically registered methods are resolved by symbol name, so they must
        // be exported. With `--dynamic-register` the symbol is private and only
        // ever referenced from the generated `RegisterNatives` table.
        if !self.method.dynamic_register {
            // `unsafe(...)` attribute syntax: required by edition 2024 and
            // accepted by earlier editions.
            self.writeln("#[unsafe(no_mangle)]");
        }
        self.fmt(format_args!(
            "pub unsafe extern \"system\" fn {jni_name}(env: d2c::Env, thiz: d2c::jni_sys::jobject"
        ));
        // Parameter names mirror Python's `p<register>`.
        let mut params = self.method.params.clone();
        if !self.ir.is_static {
            params = params.split_off(1);
        }
        for (ptype, register) in self.method.params_type.iter().zip(params.iter()) {
            let ty = Self::return_type(ptype);
            self.fmt(format_args!(", p{register}: {ty}"));
        }
        self.fmt(format_args!(
            ") -> {} {{\n",
            Self::return_type(&self.method.rtype)
        ));
        self.writeln("    unsafe {");
        self.writeln("        use dex2rs_runtime as d2c;");
        // The watchdog slot is claimed for exactly as long as this call is on
        // the stack. Without this guard the slot keeps the last state the
        // method reached -- always a `return` -- after the method has returned,
        // so any method that is called repeatedly (or a thread that has ended)
        // is reported as frozen forever, in a state the dispatcher physically
        // cannot loop in. See `watchdog::watch`.
        self.writeln("        let d2c_watch = d2c::watch();");
        // Name the method for the trace: with `--trace` every instruction is
        // logged, so without this a logcat tail shows an instruction loop with
        // no way to tell which method it came from.
        if self.ir.trace {
            self.fmt(format_args!(
                "        d2c::enter({});\n",
                bytes_literal(jni_name.as_bytes())
            ));
        }
        self.write_declarations()?;

        // move-param instructions of the entry block
        let move_params = self.ir.block(entry).move_params.clone();
        for ins in move_params {
            self.visit_move_param(ins)?;
        }

        self.fmt(format_args!(
            "        let mut __state: i64 = {};\n",
            self.ir.block(entry).num
        ));
        // Freeze guard.
        //
        // Every arm of the dispatcher ends with a jump (see
        // `block_ends_with_terminator`), so the state always changes between two
        // passes of the loop. If it does not, the arm that just ran never left:
        // the `loop` will re-run it forever, which is what hung
        // `smali/n;->invoke` (a `packed-switch` fall-through) and what a device
        // has no way of reporting on its own -- the app is not crashed, it is
        // not blocked, it simply never returns.
        //
        // After `SPIN_LIMIT` re-entries of one state the method reports itself
        // through `d2c::spin_report`, and unwinds if a Java exception is pending
        // (so ART prints the Java stack of whatever went wrong instead of the
        // app sitting on a blank screen). A legitimate self-loop in the original
        // bytecode re-executes identical straight-line code with identical
        // registers and can only exit through memory the arm itself touched, so
        // the limit cannot be reached by code that makes progress.
        self.writeln("        let mut __prev_state: i64 = i64::MIN;");
        self.writeln("        let mut __spins: u32 = 0;");
        self.writeln("        let mut __reported: bool = false;");
        self.fmt(format_args!("        {LOOP}: loop {{\n"));
        // The runtime watchdog samples this once per state transition and names
        // the method and state when the app stops making progress, which is the
        // only way a freeze inside a converted method can be diagnosed on a
        // device (a spinning thread produces no crash and no ANR backtrace).
        self.fmt(format_args!(
            "            d2c::probe({}, __state);\n",
            bytes_literal(jni_name.as_bytes())
        ));
        self.writeln("            if __state == __prev_state {");
        self.writeln("                __spins = __spins.saturating_add(1);");
        self.fmt(format_args!(
            "                if __spins >= {SPIN_LIMIT} && !__reported {{\n"
        ));
        self.writeln("                    __reported = true;");
        // The method name is not passed again here: `probe` above already
        // recorded it, and this is a cold path.
        self.writeln("                    d2c::spin_report(__state);");
        self.writeln("                    if d2c::exception_check(env) { break 'd2c; }");
        self.writeln("                }");
        self.writeln("            } else {");
        self.writeln("                __prev_state = __state;");
        self.writeln("                __spins = 0;");
        self.writeln("            }");
        self.writeln("            match __state {");

        let irblocks = self.method.irblocks.clone();
        for (index, node) in irblocks.iter().enumerate() {
            let next = irblocks.get(index + 1).copied();
            self.visit_block(*node, next)?;
        }

        // landing pads
        for pad in 0..self.ir.landing_pads.len() {
            self.visit_landing_pad(pad)?;
        }

        self.writeln("                _ => break 'd2c,");
        self.writeln("            }");
        self.writeln("        }");
        self.writeln("        // EX_UnwindBlock");
        self.fmt(format_args!("        {}\n", self.unwind_return()));
        self.writeln("    }");
        self.writeln("}");

        Ok(self.buf.clone())
    }

    fn visit_move_param(&mut self, ins: InsId) -> Result<(), String> {
        let (value, is_this) = match &self.ir.ins(ins).kind {
            InsKind::MoveParam { param } => (param.value, param.this),
            _ => return Ok(()),
        };
        let ty = self
            .ir
            .value(value)
            .get_type()
            .ok_or_else(|| "untyped parameter".to_string())?
            .to_string();
        let dest = self.var(value)?;
        if is_this {
            self.fmt(format_args!(
                "        {dest} = d2c::new_local_ref(env, thiz);\n"
            ));
        } else if util::is_ref(&ty) {
            let register = self.ir.value(value).register();
            self.fmt(format_args!(
                "        {dest} = d2c::new_local_ref(env, p{register});\n"
            ));
        } else {
            let register = self.ir.value(value).register();
            // Narrow integer parameters (`Z`/`B`/`C`/`S`) widen into their
            // `jint` slot; the C++ writer's `v0 = (jint)p1;`.
            let expr = match ty.as_str() {
                "Z" | "B" | "C" | "S" => {
                    format!("p{register} as d2c::jni_sys::jint")
                }
                _ => format!("p{register}"),
            };
            self.fmt(format_args!("        {dest} = {expr};\n"));
        }
        Ok(())
    }

    /// One arm of the dispatcher: `num => { ... }`.
    fn visit_block(
        &mut self,
        node: crate::ir::BlockId,
        next: Option<crate::ir::BlockId>,
    ) -> Result<(), String> {
        let num = self.ir.block(node).num;
        self.fmt(format_args!("            {num} => {{\n"));
        let ins_list = self.ir.block(node).ins.clone();
        for ins in &ins_list {
            self.visit_ins(*ins)?;
        }
        match next {
            Some(next) => {
                if !self.block_ends_with_terminator(&ins_list) {
                    self.emit_phi_copies(next, node)?;
                    self.fmt(format_args!(
                        "                __state = {}; continue {LOOP};\n",
                        self.ir.block(next).num
                    ));
                }
            }
            // The C++ writer emitted the arms in code order, so its last block
            // simply fell into `EX_UnwindBlock:` (the `return` after the switch).
            // A dispatcher arm has no such fall-through: ending it without
            // setting `__state` re-enters the same state forever. `throw` is the
            // instruction that makes this reachable -- it is not a terminator in
            // the writer's sense, so a method whose last block is `throw v0`
            // would spin. `break` lands on the `EX_UnwindBlock` return, exactly
            // like the generated C++ did.
            None => {
                if !self.block_ends_with_terminator(&ins_list) {
                    self.writeln("                break 'd2c; // fall into EX_UnwindBlock");
                }
            }
        }
        self.writeln("            }");
        Ok(())
    }

    /// Does the block end in a jump on every path, so no fall-through jump is
    /// needed after its last instruction?
    ///
    /// A `Switch` is deliberately **not** one of them: its cases are emitted as
    /// `if src == case { ... }` and a selector that matches none of them has to
    /// reach the DEX fall-through block (the instruction after the switch). The
    /// C++ writer got that for free by falling through to the next `L<n>:`
    /// label; in the dispatcher loop, leaving the arm without setting `__state`
    /// re-enters the same arm, which spun `smali/n;->invoke` forever.
    fn block_ends_with_terminator(&self, ins_list: &[InsId]) -> bool {
        match ins_list.last().map(|ins| &self.ir.ins(*ins).kind) {
            Some(InsKind::Goto { .. })
            | Some(InsKind::Cond { .. })
            | Some(InsKind::CondZ { .. })
            | Some(InsKind::Return { .. }) => true,
            _ => false,
        }
    }

    /// Lower the phis of `succ` that take their value from `pred`.
    fn emit_phi_copies(
        &mut self,
        succ: crate::ir::BlockId,
        pred: crate::ir::BlockId,
    ) -> Result<(), String> {
        for phi in self.ir.block(succ).phis.clone() {
            if self.ir.use_count(phi) == 0 {
                continue;
            }
            if let Some(incoming) = self.ir.phi_operand(phi, pred) {
                self.emit_move(phi, incoming)?;
            }
        }
        Ok(())
    }

    fn visit_landing_pad(&mut self, index: usize) -> Result<(), String> {
        let pad = self.ir.landing_pads[index].clone();
        let source = pad.source;
        let state = pad_state(index);
        self.fmt(format_args!("            {state} => {{\n"));
        self.writeln("                exception = d2c::get_pending_exception(env);");
        for (atype, handle) in &pad.handles {
            let name = util::get_fully_qualified_class_name(atype);
            let arg = self.name_arg(name.as_bytes())?;
            let target = self.ir.block(*handle).num;
            self.fmt(format_args!(
                "                if d2c::is_instance_of_name(env, exception, {arg}) {{\n"
            ));
            self.emit_phi_copies(*handle, source)?;
            self.fmt(format_args!(
                "                    __state = {target}; continue {LOOP};\n"
            ));
            self.writeln("                }");
        }
        self.writeln("                d2c::throw(env, exception);");
        self.writeln("                d2c::delete_local_ref(env, exception);");
        self.fmt(format_args!("                break {LOOP};\n"));
        self.writeln("            }");
        Ok(())
    }

    // ------------------------------------------------------- instructions ----

    fn visit_ins(&mut self, ins: InsId) -> Result<(), String> {
        self.write_trace(ins);
        let pad = self.pad_of(ins);
        let kind = self.ir.ins(ins).kind.clone();
        match kind {
            InsKind::Nop => {}
            InsKind::Goto { target } => {
                let node = self.node_of(target)?;
                self.emit_phi_copies(node, self.parent_of(ins)?)?;
                self.fmt(format_args!(
                    "                __state = {}; continue {LOOP};\n",
                    self.ir.block(node).num
                ));
            }
            InsKind::Cond {
                op,
                arg1,
                arg2,
                target,
            } => self.visit_cond(op, arg1, arg2, target, ins, pad)?,
            InsKind::CondZ { op, arg, target } => self.visit_cond_z(op, arg, target, ins, pad)?,
            InsKind::Return { rtype, arg } => {
                let Some(arg) = arg else {
                    self.writeln("                return;");
                    return Ok(());
                };
                let rtype = rtype.unwrap_or_else(|| "I".to_string());
                let expr = self.operand(arg)?;
                let expr = self.cast_to(rtype.as_str(), expr);
                let expr = Self::return_cast(rtype.as_str(), expr);
                self.fmt(format_args!("                return {expr};\n"));
            }
            InsKind::Move { lhs, rhs } => self.emit_move(lhs, rhs)?,
            InsKind::MoveResult { lhs, rhs } => {
                self.kill_local_ref(lhs, false)?;
                let dest = self.var(lhs)?;
                let ty = self.ir.value(lhs).get_type().unwrap_or("I").to_string();
                let expr = self.operand(rhs)?;
                let expr = self.cast_to(&ty, expr);
                self.fmt(format_args!("                {dest} = {expr};\n"));
            }
            InsKind::LoadConstant { value, cst, obfus } => {
                self.visit_load_constant(value, cst, obfus)?
            }
            InsKind::ArrayStore {
                rhs,
                array,
                index,
                elem_type,
            } => {
                let array_expr = self.var(array)?;
                self.write_not_null(&array_expr, pad)?;
                let elem = self
                    .ir
                    .array_elem_type_for(elem_type.as_deref(), array, rhs)
                    .unwrap_or_else(|| "I".to_string());
                let index_expr = self.operand(index)?;
                let value = self.operand(rhs)?;
                match Self::jni_elem_name(&elem) {
                    Some(name) => {
                        let value = self.cast_to(&elem, value);
                        self.fmt(format_args!(
                            "                d2c::Set{name}ArrayRegion(env, {array_expr}, {index_expr} as d2c::jni_sys::jsize, {value});\n"
                        ));
                    }
                    None => {
                        self.fmt(format_args!(
                            "                d2c::set_object_array_elem(env, {array_expr}, {index_expr} as d2c::jni_sys::jsize, {value});\n"
                        ));
                    }
                }
                self.write_check_pending(pad)?;
            }
            InsKind::StaticPut {
                rhs,
                clsdesc,
                ftype,
                name,
            } => self.visit_put(true, None, rhs, &clsdesc, &ftype, &name, ins, pad)?,
            InsKind::InstancePut {
                lhs,
                rhs,
                clsdesc,
                atype,
                name,
            } => self.visit_put(false, Some(lhs), rhs, &clsdesc, &atype, &name, ins, pad)?,
            InsKind::NewInstance { value, ins_type } => {
                self.kill_local_ref(value, false)?;
                let clz = self.class_slot(&ins_type)?;
                let name = util::get_type(&ins_type);
                let arg = self.name_arg(name.as_bytes())?;
                self.write_resolve_class(&clz, &arg, pad)?;
                let dest = self.var(value)?;
                self.fmt(format_args!(
                    "                {dest} = d2c::alloc_object(env, {clz});\n"
                ));
                self.write_check_pending(pad)?;
            }
            InsKind::Invoke(data) => self.visit_invoke(&data, ins, pad)?,
            InsKind::InvokePolymorphic(data) => self.visit_polymorphic_invoke(&data, ins, pad)?,
            InsKind::ConstInvoke { value, cst } => self.visit_const_invoke(value, &cst, pad)?,
            InsKind::Switch { src, cases } => {
                let src_expr = self.var(src)?;
                let parent = self.parent_of(ins)?;
                for (case, target) in &cases {
                    let node = self.node_of(*target)?;
                    self.fmt(format_args!("                if {src_expr} == {case} {{\n"));
                    self.emit_phi_copies(node, parent)?;
                    self.fmt(format_args!(
                        "                    __state = {}; continue {LOOP};\n",
                        self.ir.block(node).num
                    ));
                    self.writeln("                }");
                }
            }
            InsKind::CheckCast { arg, clsdesc } => {
                let clz = self.class_slot(&clsdesc)?;
                let name = util::get_type(&clsdesc);
                // The same argument goes to `resolve_class` and to `check_cast`
                // (which only needs it for the exception message).
                let class_arg = self.name_arg(name.as_bytes())?;
                self.write_resolve_class(&clz, &class_arg, pad)?;
                let value = self.var(arg)?;
                self.fmt(format_args!(
                    "                if d2c::check_cast(env, {value}, {clz}, {class_arg}) {{ {} }}\n",
                    self.ex_jump(pad)
                ));
            }
            InsKind::InstanceOf {
                value,
                obj,
                clsdesc,
            } => {
                let clz = self.class_slot(&clsdesc)?;
                let name = util::get_type(&clsdesc);
                let arg = self.name_arg(name.as_bytes())?;
                self.write_resolve_class(&clz, &arg, pad)?;
                let src = self.var(obj)?;
                let dest = self.var(value)?;
                self.fmt(format_args!(
                    "                {dest} = d2c::is_instance_of(env, {src}, {clz}) as d2c::jni_sys::jint;\n"
                ));
                self.write_check_pending(pad)?;
            }
            InsKind::ArrayLoad {
                value,
                array,
                index,
                elem_type,
            } => {
                let array_expr = self.var(array)?;
                self.write_not_null(&array_expr, pad)?;
                let elem = self
                    .ir
                    .array_elem_type_for(elem_type.as_deref(), array, value)
                    .unwrap_or_else(|| "I".to_string());
                let index_expr = self.operand(index)?;
                let dest = self.var(value)?;
                match Self::jni_elem_name(&elem) {
                    Some(name) => {
                        self.fmt(format_args!(
                            "                {dest} = d2c::Get{name}ArrayRegion(env, {array_expr}, {index_expr} as d2c::jni_sys::jsize) as {};\n",
                            Self::local_type(&elem)
                        ));
                    }
                    None => {
                        self.kill_local_ref(value, false)?;
                        self.fmt(format_args!(
                            "                {dest} = d2c::get_object_array_elem(env, {array_expr}, {index_expr} as d2c::jni_sys::jsize);\n"
                        ));
                    }
                }
                self.write_check_pending(pad)?;
            }
            InsKind::ArrayLength { value, array } => {
                let array_expr = self.var(array)?;
                self.write_not_null(&array_expr, pad)?;
                let dest = self.var(value)?;
                self.fmt(format_args!(
                    "                {dest} = d2c::array_length(env, {array_expr});\n"
                ));
                self.write_check_pending(pad)?;
            }
            InsKind::NewArray { value, size, atype } => {
                self.visit_new_array(value, size, &atype, ins, pad)?;
            }
            InsKind::FilledArray {
                value,
                size,
                atype,
                args,
            } => {
                self.visit_filled_array(value, size, &atype, &args, ins, pad)?;
            }
            InsKind::FillArray { reg, data } => {
                self.visit_fill_array(reg, &data)?;
            }
            InsKind::MoveException { value, .. } => {
                self.kill_local_ref(value, false)?;
                let dest = self.var(value)?;
                self.fmt(format_args!("                {dest} = exception;\n"));
            }
            InsKind::MonitorEnter { r } => {
                let value = self.var(r)?;
                self.write_not_null(&value, pad)?;
                self.fmt(format_args!(
                    "                d2c::monitor_enter(env, {value});\n"
                ));
                self.write_check_pending(pad)?;
            }
            InsKind::MonitorExit { r } => {
                let value = self.operand(r)?;
                self.fmt(format_args!(
                    "                d2c::monitor_exit(env, {value});\n"
                ));
                self.write_check_pending(pad)?;
            }
            InsKind::Throw { r } => {
                let value = self.var(r)?;
                self.write_not_null(&value, pad)?;
                self.fmt(format_args!("                d2c::throw(env, {value});\n"));
                self.write_check_pending(pad)?;
            }
            InsKind::Binary {
                op,
                value,
                arg1,
                arg2,
                op_type,
                comp,
            } => self.visit_binary(ins, op, value, arg1, arg2, &op_type, comp, pad)?,
            InsKind::Unary { value, op, arg, ty } => {
                let dest = self.var(value)?;
                let src = self.operand(arg)?;
                let c_op = op.c_op();
                self.fmt(format_args!(
                    "                {dest} = ({c_op}{src}); /* {ty} */\n"
                ));
            }
            InsKind::Cast {
                value,
                op: _,
                dest_type,
                arg,
                src_type,
            } => {
                let dest = self.var(value)?;
                let src = self.var(arg)?;
                let expr = cast_expression(&src_type, &dest_type, &src);
                self.fmt(format_args!("                {dest} = {expr};\n"));
            }
            InsKind::GetInstance {
                value,
                arg,
                ftype,
                clsdesc,
                name,
            } => {
                self.visit_get_field(false, value, Some(arg), &ftype, &clsdesc, &name, ins, pad)?
            }
            InsKind::GetStatic {
                value,
                ftype,
                clsdesc,
                name,
            } => self.visit_get_field(true, value, None, &ftype, &clsdesc, &name, ins, pad)?,
            InsKind::MoveParam { .. } => {}
        }
        Ok(())
    }

    fn parent_of(&self, ins: InsId) -> Result<crate::ir::BlockId, String> {
        self.ir
            .ins(ins)
            .parent
            .ok_or_else(|| "instruction without a parent block".to_string())
    }

    fn node_of(&self, loc: crate::ir::LocIdx) -> Result<crate::ir::BlockId, String> {
        self.ir
            .offset_to_node
            .get(&loc)
            .copied()
            .ok_or_else(|| format!("no block starts at instruction {loc}"))
    }

    fn class_slot(&self, key: &str) -> Result<String, String> {
        self.names
            .existing(key, "cls")
            .ok_or_else(|| format!("class cache slot for {key} was never allocated"))
    }

    /// One class, field or method name as generated code should pass it.
    ///
    /// A plain build gets the byte literal it always has, so its output is
    /// unchanged. Under `--obfuscate` the name is XORed here and the crate gets
    /// only the ciphertext plus the seed, exactly like a string constant: the
    /// plaintext never exists in the generated crate, which is what keeps it out
    /// of `libstub.so` and out of reach of `strings`. The runtime decodes it
    /// (`d2c::Name`), and the resolve caches are keyed on the decoded name, so an
    /// obfuscated build resolves the same classes in the same order from the same
    /// keys as a plain one.
    ///
    /// The names this covers are the ones that identify the converted code: the
    /// class/field/method names and type descriptors of the 40 530 `resolve_*`
    /// call sites of a real app, the class names `check-cast` and exception
    /// handling pass, and the element types new-object-array resolves.
    fn name_arg(&self, name: &[u8]) -> Result<String, String> {
        if !self.ir.obfus {
            return Ok(bytes_literal(name));
        }
        if name.len() >= MAX_OBF_NAME {
            return Err(format!(
                "a {} byte class/field/method name exceeds the {MAX_OBF_NAME} byte \
                 limit the runtime decodes (raise MAX_NAME in dex2rs-runtime and \
                 MAX_OBF_NAME here): {}",
                name.len(),
                String::from_utf8_lossy(name)
            ));
        }
        let (seed, cipher) = obfuscate_bytes(name);
        Ok(format!(
            "d2c::Name::obf({}, {seed:#010x})",
            bytes_literal(&cipher)
        ))
    }

    /// `D2C_RESOLVE_CLASS` — emits an early-out when resolution failed.
    ///
    /// `name` is an argument expression from [`Writer::name_arg`].
    fn write_resolve_class(
        &mut self,
        slot: &str,
        name: &str,
        pad: Option<usize>,
    ) -> Result<(), String> {
        self.fmt(format_args!(
            "                if d2c::resolve_class(env, &mut {slot}, {name}) {{ {} }}\n",
            self.ex_jump(pad)
        ));
        Ok(())
    }

    /// `D2C_RESOLVE_FIELD` / `D2C_RESOLVE_STATIC_FIELD`
    #[allow(clippy::too_many_arguments)]
    fn write_resolve_field(
        &mut self,
        cls_slot: &str,
        fld_slot: &str,
        is_static: bool,
        class_name: &str,
        field_name: &str,
        signature: &str,
        pad: Option<usize>,
    ) -> Result<(), String> {
        let class_name = self.name_arg(class_name.as_bytes())?;
        let field = self.name_arg(field_name.as_bytes())?;
        let sig = self.name_arg(signature.as_bytes())?;
        self.fmt(format_args!(
            "                if d2c::resolve_field(env, &mut {cls_slot}, &mut {fld_slot}, {is_static}, {class_name}, {field}, {sig}) {{ {} }}\n",
            self.ex_jump(pad)
        ));
        Ok(())
    }

    fn write_resolve_method(
        &mut self,
        cls_slot: &str,
        mth_slot: &str,
        is_static: bool,
        class_name: &str,
        method_name: &str,
        signature: &str,
        pad: Option<usize>,
    ) -> Result<(), String> {
        let class_name = self.name_arg(class_name.as_bytes())?;
        let method = self.name_arg(method_name.as_bytes())?;
        let sig = self.name_arg(signature.as_bytes())?;
        self.fmt(format_args!(
            "                if d2c::resolve_method(env, &mut {cls_slot}, &mut {mth_slot}, {is_static}, {class_name}, {method}, {sig}) {{ {} }}\n",
            self.ex_jump(pad)
        ));
        Ok(())
    }

    fn write_check_pending(&mut self, pad: Option<usize>) -> Result<(), String> {
        self.fmt(format_args!(
            "                if d2c::exception_check(env) {{ {} }}\n",
            self.ex_jump(pad)
        ));
        Ok(())
    }

    fn write_not_null(&mut self, expr: &str, pad: Option<usize>) -> Result<(), String> {
        self.fmt(format_args!(
            "                if d2c::not_null_or_throw(env, {expr}) {{ {} }}\n",
            self.ex_jump(pad)
        ));
        Ok(())
    }

    /// Widen an expression to the Rust type of a local of descriptor `ty`.
    fn cast_to(&self, ty: &str, expr: String) -> String {
        match ty {
            "J" => format!("({expr}) as d2c::jni_sys::jlong"),
            "F" => format!("({expr}) as d2c::jni_sys::jfloat"),
            "D" => format!("({expr}) as d2c::jni_sys::jdouble"),
            _ => expr,
        }
    }

    /// Narrow a `jint`-sized slot back to the declared JNI return type. The C++
    /// writer did the same with `return (jboolean) v1;`.
    fn return_cast(ty: &str, expr: String) -> String {
        match ty {
            "Z" => format!("({expr}) as d2c::jboolean"),
            "B" => format!("({expr}) as d2c::jni_sys::jbyte"),
            "C" => format!("({expr}) as d2c::jni_sys::jchar"),
            "S" => format!("({expr}) as d2c::jni_sys::jshort"),
            _ => expr,
        }
    }

    fn visit_cond(
        &mut self,
        op: CondOp,
        arg1: ValId,
        arg2: ValId,
        target: crate::ir::LocIdx,
        ins: InsId,
        pad: Option<usize>,
    ) -> Result<(), String> {
        let parent = self.parent_of(ins)?;
        let t = self.node_of(target)?;
        let f = self.node_of(self.ir.ins(ins).next_offset)?;
        let type1 = self.ir.value(arg1).get_type().unwrap_or("I").to_string();
        let type2 = self.ir.value(arg2).get_type().unwrap_or("I").to_string();

        let condition = if util::is_primitive_type(&type1) {
            let mut a = self.operand(arg1)?;
            let mut b = self.operand(arg2)?;
            // Rust will not compare two different widths, and inference can give
            // the two sides different ones (a `long` register against an `int`
            // one). Widen both to the larger type -- what C++ did implicitly.
            if util::is_primitive_type(&type2) {
                let common = util::get_bigger_type(&type1, &type2);
                let common = util::get_cdecl_type(&common);
                a = Self::coerce_numeric(util::get_cdecl_type(&type1), common, a);
                b = Self::coerce_numeric(util::get_cdecl_type(&type2), common, b);
            }
            let c_op = op.c_op();
            format!("({a} {c_op} {b})")
        } else {
            let a = self.var(arg1)?;
            let b = self.var(arg2)?;
            let call = format!("d2c::is_same_object(env, {a}, {b})");
            match op {
                CondOp::NEqual => format!("(!{call})"),
                _ => call,
            }
        };

        self.fmt(format_args!("                if {condition} {{\n"));
        self.emit_phi_copies(t, parent)?;
        self.fmt(format_args!(
            "                    __state = {}; continue {LOOP};\n",
            self.ir.block(t).num
        ));
        self.writeln("                } else {");
        self.emit_phi_copies(f, parent)?;
        self.fmt(format_args!(
            "                    __state = {}; continue {LOOP};\n",
            self.ir.block(f).num
        ));
        self.writeln("                }");
        let _ = pad;
        Ok(())
    }

    fn visit_cond_z(
        &mut self,
        op: CondOp,
        arg: ValId,
        target: crate::ir::LocIdx,
        ins: InsId,
        pad: Option<usize>,
    ) -> Result<(), String> {
        let parent = self.parent_of(ins)?;
        let t = self.node_of(target)?;
        let f = self.node_of(self.ir.ins(ins).next_offset)?;
        let atype = self.ir.value(arg).get_type().unwrap_or("I").to_string();
        let value = self.operand(arg)?;

        let condition = if util::is_primitive_type(&atype) {
            let c_op = op.c_op();
            format!("({value} {c_op} 0)")
        } else {
            match op {
                CondOp::Equal => format!("{value}.is_null()"),
                CondOp::NEqual => format!("(!{value}.is_null())"),
                _ => {
                    let c_op = op.c_op();
                    format!("({value} {c_op} core::ptr::null_mut())")
                }
            }
        };

        self.fmt(format_args!("                if {condition} {{\n"));
        self.emit_phi_copies(t, parent)?;
        self.fmt(format_args!(
            "                    __state = {}; continue {LOOP};\n",
            self.ir.block(t).num
        ));
        self.writeln("                } else {");
        self.emit_phi_copies(f, parent)?;
        self.fmt(format_args!(
            "                    __state = {}; continue {LOOP};\n",
            self.ir.block(f).num
        ));
        self.writeln("                }");
        let _ = pad;
        Ok(())
    }

    fn visit_load_constant(&mut self, value: ValId, cst: ValId, obfus: bool) -> Result<(), String> {
        self.kill_local_ref(value, false)?;
        let dest = self.var(value)?;
        let atype = self.ir.value(value).get_type().unwrap_or("I").to_string();
        let cst_type = self.ir.value(cst).get_type().unwrap_or("").to_string();

        if atype == "F" {
            let bits = self.ir.constant_int(cst).unwrap_or(0);
            self.fmt(format_args!(
                "                {dest} = d2c::bitcast_to_float({bits});\n"
            ));
        } else if atype == "D" {
            let bits = self.ir.constant_int(cst).unwrap_or(0);
            self.fmt(format_args!(
                "                {dest} = d2c::bitcast_to_double({bits});\n"
            ));
        } else if cst_type == "Ljava/lang/String;" {
            let raw = self.ir.constant_string(cst).unwrap_or_default();
            // `NewStringUTF` takes modified UTF-8, not the UTF-8 a Rust string
            // holds; see `util::mutf8` for why the difference is visible.
            let mut bytes = util::mutf8(&raw);
            if obfus {
                bytes.push(0);
                // Obfuscated *here*, not in the generated crate. Calling
                // `d2c::obfuscate(b"...", KEY)` does compute the ciphertext at
                // compile time, but rustc still materialises the `b"..."`
                // argument as its own anonymous constant and fat LTO does not
                // remove it: `strings` on the test app's `libstub.so` read 695
                // of its 1976 obfuscated constants in plain text. Emitting the
                // ciphertext keeps the plaintext inside this process, and the
                // per-string seed keeps one known key from decoding the lot.
                let (seed, cipher) = obfuscate_bytes(&bytes);
                let literal = bytes_literal(&cipher);
                self.fmt(format_args!(
                    "                {dest} = d2c::new_string_utf_obf_seeded(env, {literal}, {seed:#010x});\n"
                ));
            } else {
                let literal = bytes_literal(&bytes);
                self.fmt(format_args!(
                    "                {dest} = d2c::new_string_utf(env, {literal});\n"
                ));
            }
        } else if cst_type == "Ljava/lang/Class;" {
            let raw = self.ir.constant_string(cst).unwrap_or_default();
            // `LoadConstant.get_class()` keys the cache on the *hex-escaped* name,
            // which is what the declaration pass collected.
            let clz = self.class_slot(&util::hex_escape_string(&raw))?;
            let arg = self.name_arg(raw.as_bytes())?;
            let pad = None;
            self.write_resolve_class(&clz, &arg, pad)?;
            self.fmt(format_args!(
                "                {dest} = d2c::new_local_ref(env, {clz});\n"
            ));
        } else {
            let literal = self.ir.constant_literal(cst);
            self.fmt(format_args!(
                "                {dest} = {literal} as {};\n",
                Self::local_type(&atype)
            ));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn visit_put(
        &mut self,
        is_static: bool,
        lhs: Option<ValId>,
        rhs: ValId,
        clsdesc: &str,
        ftype: &str,
        name: &str,
        ins: InsId,
        pad: Option<usize>,
    ) -> Result<(), String> {
        if let Some(lhs) = lhs {
            let value = self.var(lhs)?;
            self.write_not_null(&value, pad)?;
        }
        let clz = self.class_slot(clsdesc)?;
        let fld = self.field_slot(clsdesc, name)?;
        let class_name = util::get_type(clsdesc);
        let _ = ins;
        self.write_resolve_field(&clz, &fld, is_static, &class_name, name, ftype, pad)?;
        let value = self.operand(rhs)?;
        let value = self.cast_to(ftype, value);
        match Self::jni_elem_name(ftype) {
            Some(field) => {
                if is_static {
                    self.fmt(format_args!(
                        "                d2c::SetStatic{field}Field(env, {clz}, {fld}, {value});\n"
                    ));
                } else {
                    let obj = self.var(lhs.ok_or("instance field without an object")?)?;
                    self.fmt(format_args!(
                        "                d2c::Set{field}Field(env, {obj}, {fld}, {value});\n"
                    ));
                }
            }
            None => {
                if is_static {
                    self.fmt(format_args!(
                        "                d2c::set_static_object_field(env, {clz}, {fld}, {value});\n"
                    ));
                } else {
                    let obj = self.var(lhs.ok_or("instance field without an object")?)?;
                    self.fmt(format_args!(
                        "                d2c::set_object_field(env, {obj}, {fld}, {value});\n"
                    ));
                }
            }
        }
        self.write_check_pending(pad)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn visit_get_field(
        &mut self,
        is_static: bool,
        value: ValId,
        arg: Option<ValId>,
        ftype: &str,
        clsdesc: &str,
        name: &str,
        _ins: InsId,
        pad: Option<usize>,
    ) -> Result<(), String> {
        if let Some(arg) = arg {
            let obj = self.var(arg)?;
            self.write_not_null(&obj, pad)?;
        }
        self.kill_local_ref(value, false)?;
        let clz = self.class_slot(clsdesc)?;
        let fld = self.field_slot(clsdesc, name)?;
        let class_name = util::get_type(clsdesc);
        self.write_resolve_field(&clz, &fld, is_static, &class_name, name, ftype, pad)?;
        let dest = self.var(value)?;
        match Self::jni_elem_name(ftype) {
            Some(field) => {
                let accessor = if is_static {
                    format!("GetStatic{field}Field(env, {clz}, {fld})")
                } else {
                    let obj = self.var(arg.ok_or("instance field without an object")?)?;
                    format!("Get{field}Field(env, {obj}, {fld})")
                };
                let widened = Self::widen(ftype, format!("d2c::{accessor}"));
                self.fmt(format_args!(
                    "                {dest} = {widened} as {};\n",
                    Self::local_type(ftype)
                ));
            }
            None => {
                let accessor = if is_static {
                    format!("get_static_object_field(env, {clz}, {fld})")
                } else {
                    let obj = self.var(arg.ok_or("instance field without an object")?)?;
                    format!("get_object_field(env, {obj}, {fld})")
                };
                self.fmt(format_args!("                {dest} = d2c::{accessor};\n"));
            }
        }
        self.write_check_pending(pad)?;
        Ok(())
    }

    fn field_slot(&self, clsdesc: &str, name: &str) -> Result<String, String> {
        let key = format!("{clsdesc}.{name}");
        self.names
            .existing(&key, "fld")
            .ok_or_else(|| format!("field cache slot for {key} was never allocated"))
    }

    fn method_slot(&self, key: &str) -> Result<String, String> {
        self.names
            .existing(key, "mth")
            .ok_or_else(|| format!("method cache slot for {key} was never allocated"))
    }

    fn visit_invoke(
        &mut self,
        data: &crate::ir::InvokeData,
        _ins: InsId,
        pad: Option<usize>,
    ) -> Result<(), String> {
        if let Some(thiz) = data.thiz {
            let base = self.var(thiz)?;
            self.write_not_null(&base, pad)?;
        }
        let clz = self.class_slot(&data.clsdesc)?;
        let key = format!("{}->{}({})", data.clsdesc, data.name, data.ptype.join(""));
        let mid = self.method_slot(&key)?;
        let class_name = util::get_type(&data.clsdesc);
        let signature = format!("({}){}", data.ptype.join(""), data.rtype);
        self.write_resolve_method(
            &clz,
            &mid,
            data.invoke_type.is_static(),
            &class_name,
            &data.name,
            &signature,
            pad,
        )?;

        // `jvalue args[] = { ... }`
        let mut slots = Vec::new();
        for (arg, atype) in data.args.iter().zip(data.ptype.iter()) {
            let expr = self.operand(*arg)?;
            slots.push(format!(
                "d2c::{}({})",
                Self::jvalue_ctor(atype),
                self.cast_to(atype, expr)
            ));
        }
        self.fmt(format_args!(
            "                let __args: [d2c::jni_sys::jvalue; {}] = [{}];\n",
            slots.len(),
            slots.join(", ")
        ));

        let call_style = match data.invoke_type {
            crate::ir::InvokeType::Static => "d2c::CALL_STATIC",
            crate::ir::InvokeType::Super => "d2c::CALL_NONVIRTUAL",
            _ => "d2c::CALL_VIRTUAL",
        };
        let obj = match data.thiz {
            Some(thiz) => self.var(thiz)?,
            None => "core::ptr::null_mut()".to_string(),
        };
        let kind = Self::descriptor(&data.rtype);
        let name = match kind {
            "Void" => "call_void_method".to_string(),
            other => format!("call_{}_method", other.to_ascii_lowercase()),
        };
        let call = format!("d2c::{name}(env, {call_style}, {obj}, {clz}, {mid}, __args.as_ptr())");
        match data.result {
            Some(result) => {
                // `write_kill_local_reference(ins.get_value())` — the reference
                // deletes the *destination* slot's previous ref here, but only
                // when that value is register-backed (`tmp=False`). It is a
                // no-op for the synthesized result temporaries, whose slot may
                // still alias a live handle (`v4 = v3` from a `move-result`):
                // deleting it here freed a reference that a later instruction
                // still handed to the VM. The dead-result cleanup happens after
                // the call, once the slot holds the freshly returned ref.
                self.kill_local_ref(result, false)?;
                let dest = self.var(result)?;
                let cast = Self::widen(&data.rtype, call);
                self.fmt(format_args!(
                    "                {dest} = {cast} as {};\n",
                    Self::local_type(&data.rtype)
                ));
            }
            None => {
                self.fmt(format_args!("                {call};\n"));
            }
        }
        self.write_check_pending(pad)?;
        if let Some(result) = data.result {
            self.delete_dead_local_ref(result)?;
        }
        Ok(())
    }

    /// `invoke-polymorphic` — one runtime call plus one unboxing call.
    ///
    /// The arguments are the same `jvalue[]` an ordinary invoke builds; the
    /// runtime boxes them per the call-site prototype. A primitive result comes
    /// back boxed, so it is unboxed with the accessor matching the prototype's
    /// return descriptor — the same shape `call_*_method` would have had, which
    /// keeps the destination's widening identical to a normal call.
    fn visit_polymorphic_invoke(
        &mut self,
        data: &crate::ir::PolymorphicInvokeData,
        _ins: InsId,
        pad: Option<usize>,
    ) -> Result<(), String> {
        let target = self.var(data.target)?;
        self.write_not_null(&target, pad)?;

        let mut slots = Vec::new();
        for (arg, atype) in data.args.iter().zip(data.ptype.iter()) {
            let expr = self.operand(*arg)?;
            slots.push(format!(
                "d2c::{}({})",
                Self::jvalue_ctor(atype),
                self.cast_to(atype, expr)
            ));
        }
        self.fmt(format_args!(
            "                let __args: [d2c::jni_sys::jvalue; {}] = [{}];\n",
            slots.len(),
            slots.join(", ")
        ));

        let name = self.name_arg(data.name.as_bytes())?;
        let proto = self.name_arg(format!("({}){}", data.ptype.join(""), data.rtype).as_bytes())?;
        let call = format!(
            "d2c::invoke_polymorphic(env, {target}, {}, {name}, {proto}, __args.as_ptr())",
            data.kind.runtime_const()
        );

        match data.result {
            Some(result) => {
                self.kill_local_ref(result, false)?;
                let dest = self.var(result)?;
                let value = match Self::descriptor(&data.rtype) {
                    // A reference result is already the object the runtime
                    // returned; there is nothing to unbox.
                    "Object" => call,
                    other => format!("d2c::unbox_{}(env, {call})", other.to_ascii_lowercase()),
                };
                let cast = Self::widen(&data.rtype, value);
                self.fmt(format_args!(
                    "                {dest} = {cast} as {};\n",
                    Self::local_type(&data.rtype)
                ));
            }
            None => {
                self.fmt(format_args!("                {call};\n"));
            }
        }
        self.write_check_pending(pad)?;
        if let Some(result) = data.result {
            self.delete_dead_local_ref(result)?;
        }
        Ok(())
    }

    /// The DEX `handle_type` as the runtime's constant, so the two crates cannot
    /// disagree about what `0x04` means.
    fn handle_kind_const(kind: u8) -> &'static str {
        match kind {
            0x00 => "d2c::HANDLE_KIND_STATIC_PUT",
            0x01 => "d2c::HANDLE_KIND_STATIC_GET",
            0x02 => "d2c::HANDLE_KIND_INSTANCE_PUT",
            0x03 => "d2c::HANDLE_KIND_INSTANCE_GET",
            0x04 => "d2c::HANDLE_KIND_INVOKE_STATIC",
            0x05 => "d2c::HANDLE_KIND_INVOKE_INSTANCE",
            0x06 => "d2c::HANDLE_KIND_INVOKE_DIRECT",
            0x07 => "d2c::HANDLE_KIND_INVOKE_CONSTRUCTOR",
            _ => "d2c::HANDLE_KIND_INVOKE_INTERFACE",
        }
    }

    /// `const-method-type` / `const-method-handle`: one runtime call producing a
    /// fresh object.
    fn visit_const_invoke(
        &mut self,
        value: ValId,
        cst: &crate::ir::ConstInvokeValue,
        pad: Option<usize>,
    ) -> Result<(), String> {
        let call = match cst {
            crate::ir::ConstInvokeValue::MethodType { proto } => format!(
                "d2c::method_type(env, {})",
                self.name_arg(proto.as_bytes())?
            ),
            crate::ir::ConstInvokeValue::MethodHandle {
                kind,
                clsdesc,
                name,
                desc,
            } => format!(
                "d2c::method_handle(env, {},
                    {},
                    {},
                    {})",
                Self::handle_kind_const(*kind),
                self.name_arg(clsdesc.as_bytes())?,
                self.name_arg(name.as_bytes())?,
                self.name_arg(desc.as_bytes())?
            ),
        };
        self.kill_local_ref(value, false)?;
        let dest = self.var(value)?;
        self.fmt(format_args!(
            "                {dest} = {call} as d2c::jni_sys::jobject;\n"
        ));
        self.write_check_pending(pad)?;
        self.delete_dead_local_ref(value)?;
        Ok(())
    }

    fn visit_new_array(
        &mut self,
        value: ValId,
        size: ValId,
        atype: &str,
        _ins: InsId,
        pad: Option<usize>,
    ) -> Result<(), String> {
        let elem = atype[1..].to_string();
        let size_expr = self.operand(size)?;
        self.write_array_size_check(&size_expr, pad)?;
        self.kill_local_ref(value, false)?;
        let dest = self.var(value)?;
        match Self::jni_elem_name(&elem) {
            Some(name) => {
                self.fmt(format_args!(
                    "                {dest} = d2c::New{name}Array(env, {size_expr} as d2c::jni_sys::jsize);\n"
                ));
            }
            None => {
                let clz = self.class_slot(&elem)?;
                let name = util::get_type(&elem);
                let arg = self.name_arg(name.as_bytes())?;
                self.write_resolve_class(&clz, &arg, pad)?;
                self.fmt(format_args!(
                    "                {dest} = d2c::new_object_array(env, {size_expr} as d2c::jni_sys::jsize, {clz});\n"
                ));
            }
        }
        self.write_check_pending(pad)?;
        Ok(())
    }

    fn write_array_size_check(
        &mut self,
        size_expr: &str,
        pad: Option<usize>,
    ) -> Result<(), String> {
        self.fmt(format_args!("                if {size_expr} < 0 {{\n"));
        self.writeln(
            "                    d2c::throw_exception(env, b\"java/lang/NegativeArraySizeException\", b\"negative array size\");",
        );
        self.fmt(format_args!("                    {}\n", self.ex_jump(pad)));
        self.writeln("                }");
        Ok(())
    }

    fn visit_filled_array(
        &mut self,
        value: ValId,
        size: u32,
        atype: &str,
        args: &[ValId],
        _ins: InsId,
        pad: Option<usize>,
    ) -> Result<(), String> {
        self.kill_local_ref(value, false)?;
        let elem = atype[1..].to_string();
        let dest = self.var(value)?;
        match Self::jni_elem_name(&elem) {
            Some(name) => {
                self.fmt(format_args!(
                    "                {dest} = d2c::New{name}Array(env, {size} as d2c::jni_sys::jsize);\n"
                ));
                let mut values = Vec::new();
                for arg in args {
                    let expr = self.operand(*arg)?;
                    values.push(self.cast_to(&elem, expr));
                }
                self.fmt(format_args!(
                    "                d2c::set_{}_array(env, {dest}, &[{}]);\n",
                    name.to_lowercase(),
                    values.join(", ")
                ));
            }
            None => {
                if elem == "J" || elem == "D" {
                    return Err("filled-new-array of a wide element type".to_string());
                }
                let clz = self.class_slot(&elem)?;
                let name = util::get_type(&elem);
                let arg = self.name_arg(name.as_bytes())?;
                self.write_resolve_class(&clz, &arg, pad)?;
                self.fmt(format_args!(
                    "                {dest} = d2c::new_object_array(env, {size} as d2c::jni_sys::jsize, {clz});\n"
                ));
                let mut values = Vec::new();
                for arg in args {
                    values.push(self.operand(*arg)?);
                }
                self.fmt(format_args!(
                    "                d2c::set_object_array_region(env, {dest}, &[{}]);\n",
                    values.join(", ")
                ));
            }
        }
        self.write_check_pending(pad)?;
        Ok(())
    }

    /// `visit_fill_array()` — the payload bytes are turned into a typed slice.
    fn visit_fill_array(&mut self, reg: ValId, data: &crate::ir::ArrayData) -> Result<(), String> {
        let array_type = self
            .ir
            .value(reg)
            .get_type()
            .ok_or_else(|| "fill-array-data on an untyped array".to_string())?
            .to_string();
        if !util::is_array(&array_type) {
            return Err("fill-array-data on a non-array".to_string());
        }
        let elem = array_type[1..].to_string();
        let name = Self::elem_name(&elem)
            .ok_or_else(|| "fill-array-data on an object array".to_string())?;
        let width = data.element_width.max(1) as usize;
        let count = data.bytes.len() / width;

        let mut values = Vec::with_capacity(count);
        for index in 0..count {
            let chunk = &data.bytes[index * width..(index + 1) * width];
            let mut raw = [0u8; 8];
            raw[..width.min(8)].copy_from_slice(&chunk[..width.min(8)]);
            let le = u64::from_le_bytes(raw);
            values.push(match elem.as_str() {
                "Z" => (le != 0).to_string(),
                "B" => format!("{}", le as u8 as i8),
                "C" => format!("{}", le as u16),
                "S" => format!("{}", le as i16),
                "I" => format!("{}", le as i32),
                "J" => format!("{}", le as i64),
                "F" => format!("d2c::bitcast_to_float({})", le as i64),
                _ => format!("d2c::bitcast_to_double({})", le as i64),
            });
        }

        let dest = self.var(reg)?;
        self.fmt(format_args!(
            "                d2c::set_{name}_array(env, {dest}, &[{}]);\n",
            values.join(", ")
        ));
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn visit_binary(
        &mut self,
        ins: InsId,
        op: BinOp,
        value: ValId,
        arg1: ValId,
        arg2: ValId,
        op_type: &str,
        _comp: bool,
        pad: Option<usize>,
    ) -> Result<(), String> {
        let dest = self.var(value)?;
        if op.needs_div_guard() && !util::is_float(op_type) {
            let divisor = self.operand(arg2)?;
            self.fmt(format_args!("                if {divisor} == 0 {{\n"));
            self.writeln(
                "                    d2c::throw_exception(env, b\"java/lang/ArithmeticException\", b\"divide by zero\");",
            );
            self.fmt(format_args!("                    {}\n", self.ex_jump(pad)));
            self.writeln("                }");
        }
        let _ = ins;

        let a = self.operand(arg1)?;
        let b = self.operand(arg2)?;
        let expr = match op {
            BinOp::Cmp | BinOp::Cmpl => {
                format!("if {a} == {b} {{ 0 }} else if {a} > {b} {{ 1 }} else {{ -1 }}")
            }
            BinOp::Cmpg => {
                format!("if {a} == {b} {{ 0 }} else if {a} < {b} {{ -1 }} else {{ 1 }}")
            }
            BinOp::InShr => format!("(({a}) >> (({b}) & 0x1f))"),
            BinOp::InShl => format!("(({a}) << (({b}) & 0x1f))"),
            BinOp::InUShr => format!("(((({a}) as u32) >> (({b}) & 0x1f)) as d2c::jni_sys::jint)"),
            BinOp::LongShr => format!("(({a}) >> (({b}) & 0x3f))"),
            BinOp::LongShl => format!("(({a}) << (({b}) & 0x3f))"),
            BinOp::LongUShr => {
                format!("(((({a}) as u64) >> (({b}) & 0x3f)) as d2c::jni_sys::jlong)")
            }
            BinOp::ModF | BinOp::ModD => format!("({a} % {b})"),
            other => format!("({a} {} {b})", other.c_op()),
        };
        let expr = if _comp {
            format!("({expr}) as d2c::jni_sys::jint")
        } else {
            expr
        };
        self.fmt(format_args!("                {dest} = {expr};\n"));
        Ok(())
    }
}

/// Landing pad `i` gets the state number `-(i + 1)`.
fn pad_state(index: usize) -> i64 {
    -(index as i64) - 1
}

/// The longest class/field/method name `--obfuscate` will encrypt, and therefore
/// the longest the runtime is asked to decode.
///
/// A name at or past this length is refused at generation time rather than
/// truncated, because a truncated name would resolve to nothing and look like a
/// missing class. The two ends have to agree and nothing in the type system says
/// so, which is why `dex2rs-core/tests/compile.rs` asserts this equals
/// `dex2rs_runtime::MAX_NAME`. On the test app the longest name is 468 bytes, so
/// this is a bound rather than a limit anyone meets.
pub const MAX_OBF_NAME: usize = 1024;

/// The seed `--obfuscate` picks for one string constant, from the string itself.
///
/// Derived rather than random so regenerating an unchanged app produces
/// byte-identical output, and mixed so that changing one byte of a string moves
/// every byte of its keystream. The runtime never needs this function: the seed
/// travels with the constant, and `dex2rs_runtime::keystream` turns it back into
/// the same keystream.
pub fn keystream_seed(data: &[u8]) -> u32 {
    // FNV-1a, then the splitmix64 finaliser: FNV alone leaves the low bits of
    // short inputs poorly mixed, and a keystream is only as good as its seed.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut z = hash;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    (z ^ (z >> 31)) as u32
}

/// The keystream for `seed`, mirroring `dex2rs_runtime::keystream`.
///
/// The two have to agree byte for byte and nothing in the type system says so, so
/// `obfuscated_literals_use_the_runtime_keystream` in `tests/compile.rs` decodes a
/// generated literal with the runtime's own generator and fails if they drift.
pub fn keystream(seed: u32, len: usize) -> Vec<u8> {
    let mut keys = vec![0u8; len];
    let mut x = seed;
    let mut i = 0;
    while i < len {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        let mut b = 0;
        while b < 4 && i + b < len {
            keys[i + b] = (x >> (b * 8)) as u8;
            b += 1;
        }
        i += 4;
    }
    keys
}

/// `AY_OBFUSCATE`, applied while generating: XOR `data` with the keystream its
/// own seed produces, which is what `dex2rs_runtime::new_string_utf_obf_seeded`
/// XORs back off.
pub fn obfuscate_bytes(data: &[u8]) -> (u32, Vec<u8>) {
    let seed = keystream_seed(data);
    let keys = keystream(seed, data.len());
    let cipher = data
        .iter()
        .zip(keys)
        .map(|(byte, key)| byte ^ key)
        .collect();
    (seed, cipher)
}

/// A Rust byte-string literal for arbitrary bytes.
pub fn bytes_literal(bytes: &[u8]) -> String {
    let mut out = String::from("b\"");
    for byte in bytes {
        match *byte {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(*byte as char),
            other => out.push_str(&format!("\\x{other:02x}")),
        }
    }
    out.push('"');
    out
}

/// `visit_cast()` — the four conversions the C++ handled with helpers, the rest
/// with a plain C cast.
fn cast_expression(src_type: &str, dest_type: &str, src: &str) -> String {
    match (src_type, dest_type) {
        ("D", "J") => format!("d2c::double_to_long({src})"),
        ("D", "I") => format!("d2c::double_to_int({src})"),
        ("F", "J") => format!("d2c::float_to_long({src})"),
        ("F", "I") => format!("d2c::float_to_int({src})"),
        (_, "J") => format!("({src}) as d2c::jni_sys::jlong"),
        (_, "I") | (_, "B") | (_, "C") | (_, "S") => {
            format!("({src}) as d2c::jni_sys::jint")
        }
        (_, "F") => format!("({src}) as d2c::jni_sys::jfloat"),
        (_, "D") => format!("({src}) as d2c::jni_sys::jdouble"),
        _ => src.to_string(),
    }
}
