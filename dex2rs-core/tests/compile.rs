//! End-to-end compiler tests: smali text -> dex -> Rust source.
//!
//! The fixtures cover the shapes the C++ writer was built around (arithmetic,
//! branches, loops, string/class constants, arrays, fields, invokes,
//! try/catch) plus the phi-node lowering the Python tool never had.

use dex2rs_core::dexinput::decode_method;
use dex2rs_core::ssa::IrBuilder;
use smali::dex::DexFile;
use smali::types::SmaliClass;

const HELLO: &str = include_str!("fixtures/Hello.smali");
const POLYMORPHIC: &str = include_str!("fixtures/Polymorphic.smali");
const INVOKE_CONST: &str = include_str!("fixtures/InvokeConst.smali");

fn class() -> SmaliClass {
    SmaliClass::from_smali(HELLO).expect("fixture parses")
}

fn polymorphic_class() -> SmaliClass {
    SmaliClass::from_smali(POLYMORPHIC).expect("polymorphic fixture parses")
}

fn invoke_const_class() -> SmaliClass {
    SmaliClass::from_smali(INVOKE_CONST).expect("invoke-const fixture parses")
}

/// Compile one method to Rust source.
fn compile(source: &SmaliClass, name: &str) -> String {
    let method = source
        .methods
        .iter()
        .find(|m| m.name == name)
        .unwrap_or_else(|| panic!("fixture has a method named {name}"));
    let input = decode_method(source, method, false, false).expect("decode");
    let builder = IrBuilder::new(&input);
    let irmethod = builder.process().expect("compile");
    irmethod.get_source().expect("codegen")
}

/// A `packed-switch` whose selector matches no case must jump to the block that
/// follows it in the DEX. Without that jump the dispatcher re-enters the same
/// arm forever — the bug that hung `ui/components/smali/n;->invoke` on device
/// (its selector was outside the case range of R8's `when` table).
#[test]
fn switch_falls_through_when_no_case_matches() {
    let rust = compile(&class(), "classify");
    assert!(rust.contains("if v0 == 0 {"), "{rust}");
    assert!(rust.contains("if v0 == 1 {"), "{rust}");

    // Everything between the last case and the end of the match arm must set
    // `__state`, so a non-matching selector leaves the arm.
    let last_case = rust.rfind("if v0 == 1 {").expect("second case");
    let tail = &rust[last_case..];
    let arm_end = tail
        .find("\n            }\n")
        .expect("end of the match arm");
    let tail = &tail[..arm_end];
    assert!(
        tail.contains("__state = "),
        "the switch arm ends without a fall-through jump:\n{tail}"
    );
}

/// A method whose last block ends in `throw` has no successor in the DEX. The
/// C++ writer emitted its arms in code order, so that block fell into
/// `EX_UnwindBlock:` and returned; a dispatcher arm has no such fall-through, so
/// without an explicit `break` the arm ends, the `loop` re-enters the same state
/// and the method spins on the device (the app is neither crashed nor blocked --
/// it just never returns, which is exactly the "grey screen" report).
#[test]
fn last_block_throw_falls_into_ex_unwind_block() {
    let rust = compile(&class(), "boom");
    assert!(rust.contains("d2c::throw(env, v0);"), "{rust}");
    assert!(
        rust.contains("break 'd2c; // fall into EX_UnwindBlock"),
        "the last arm has no way out of the dispatcher:\n{rust}"
    );
}

/// Decode one of the generated `b"..."` byte literals back to bytes, without
/// reusing the emitter's own escaping helper (`codegen::bytes_literal`), so the
/// test cannot agree with a buggy emitter by construction.
fn decode_byte_literal(literal: &str) -> Vec<u8> {
    let body = literal
        .strip_prefix("b\"")
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or_else(|| panic!("not a byte literal: {literal}"));
    let mut out = Vec::new();
    let mut chars = body.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next().expect("escape at end of literal") {
            '\\' => out.push(b'\\'),
            '"' => out.push(b'"'),
            'n' => out.push(b'\n'),
            'r' => out.push(b'\r'),
            't' => out.push(b'\t'),
            'x' => {
                let hi = chars.next().expect("hex digit");
                let lo = chars.next().expect("hex digit");
                let byte = u8::from_str_radix(&format!("{hi}{lo}"), 16).expect("hex byte");
                out.push(byte);
            }
            other => panic!("unexpected escape \\{other} in {literal}"),
        }
    }
    out
}

/// The `b"..."` literal that follows `needle`, plus everything after its
/// closing quote. Walking the body rather than taking the first quote matters:
/// the source of a literal byte is itself backslash-escaped here, so a plain
/// `find('"')` would stop inside `\\x22` and read the escapes as the seed.
fn byte_literal_after<'a>(rust: &'a str, needle: &str) -> (&'a str, &'a str) {
    let start = rust
        .find(needle)
        .unwrap_or_else(|| panic!("no {needle} in:\n{rust}"))
        + needle.len();
    let rest = &rust[start..];
    let body = rest
        .strip_prefix("b\"")
        .unwrap_or_else(|| panic!("no byte literal after {needle} in:\n{rust}"));
    let mut escaped = false;
    let mut end = None;
    for (i, ch) in body.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => {
                end = Some(i);
                break;
            }
            _ => {}
        }
    }
    let end = end.expect("unterminated literal");
    (&body[..end], &body[end + 1..])
}

/// The first `b"..."` literal passed to `needle` in `rust`, decoded.
fn literal_after(rust: &str, needle: &str) -> Vec<u8> {
    let (body, _) = byte_literal_after(rust, needle);
    decode_byte_literal(&format!("b\"{body}\""))
}

/// `DexOp::ConstString.value` is escaped smali text, and the generated code has
/// to unescape it exactly once. Leaving it raw made every string containing an
/// escape reach the app with the escape characters still in it: a real app sent
/// SQLite `\n        UPDATE build_jobs ...` and got
/// `unrecognized token: "\" (code 1 SQLITE_ERROR)`.
#[test]
fn string_escapes_are_decoded_once() {
    let rust = compile(&class(), "escaped");
    let decoded = literal_after(&rust, "d2c::new_string_utf(env, ");
    assert_eq!(
        decoded,
        "a\nb\tc\"d\\e\u{e9}".as_bytes(),
        "the emitted literal must hold the Java string itself, not its smali \
         escaping: {decoded:?}"
    );
}

/// The freeze guard is emitted into every method: `d2c::spin_report` names the
/// method and state if the dispatcher ever re-enters a state without leaving it,
/// which is how a freeze is diagnosed without a `--trace` build.
///
/// The `d2c::watch()` guard is the other half and is just as load-bearing: the
/// watchdog slot has to be handed back when the method returns, or a method that
/// has returned is indistinguishable from one that is still running, and a
/// freeze is reported in a state the dispatcher cannot loop in.
#[test]
fn every_method_has_the_freeze_guard() {
    let rust = compile(&class(), "add");
    assert!(rust.contains("d2c::spin_report(__state);"), "{rust}");
    assert!(rust.contains("if __state == __prev_state {"), "{rust}");
    assert!(
        rust.contains("d2c::probe(b\"Java_com_example_Hello_add__II\", __state);"),
        "the watchdog must be told which method is running: {rust}"
    );
    assert!(
        rust.contains("let d2c_watch = d2c::watch();"),
        "the watchdog slot must be held for exactly as long as the method is on \
         the stack: {rust}"
    );
}

#[test]
fn compiles_every_fixture_method() {
    let source = class();
    for method in &source.methods {
        let input = decode_method(&source, method, false, false).expect("decode");
        let irmethod = IrBuilder::new(&input)
            .process()
            .unwrap_or_else(|e| panic!("{}: {e}", method.name));
        let rust = irmethod
            .get_source()
            .unwrap_or_else(|e| panic!("{}: {e}", method.name));
        // The generated body is a dispatcher loop with a JNI entry point.
        assert!(
            rust.contains("extern \"system\" fn Java_com_example_Hello_"),
            "{rust}"
        );
        assert!(rust.contains("'d2c: loop"), "{rust}");
        assert!(rust.contains("d2c::spin_report("), "{rust}");
    }
}

#[test]
fn simple_arithmetic() {
    // Reference (`tools/golden_cpp.py`, method `add`):
    //   jint v0; jint v1; jint v2;
    //   v0 = (jint)p1;  v1 = (jint)p2;
    //   v2 = (v0 + v1);  return (jint) v2;
    // Register slots keep the same numbering as the C++ writer: the parameter
    // registers are declared first (r1, r2), then the result register (r0).
    let rust = compile(&class(), "add");
    assert!(rust.contains("let mut __state: i64 = 0;"), "{rust}");
    assert!(rust.contains("v0 = p1;"), "{rust}");
    assert!(rust.contains("v1 = p2;"), "{rust}");
    assert!(rust.contains("v2 = (v0 + v1);"), "{rust}");
    assert!(rust.contains("return v2;"), "{rust}");
}

/// An instance method with no parameters still has a `this` register: the C++
/// writer pushed it unconditionally, and so must we (a real APK hit this with
/// `()V` instance methods).
#[test]
fn instance_method_without_parameters_keeps_this() {
    let rust = compile(&class(), "instanceOnly");
    assert!(
        rust.contains("fn Java_com_example_Hello_instanceOnly__("),
        "{rust}"
    );
    assert!(
        rust.contains("v0 = d2c::new_local_ref(env, thiz);"),
        "{rust}"
    );
    assert!(rust.contains("v1 = 5 as d2c::jni_sys::jint;"), "{rust}");
    assert!(rust.contains("return v1;"), "{rust}");
}

#[test]
fn jni_signature_matches_art() {
    let rust = compile(&class(), "add");
    assert!(
        rust.contains("fn Java_com_example_Hello_add__II("),
        "long JNI name with the parameter encoding: {rust}"
    );
    // Parameters are named after the register that holds them (`p1`, `p2`), the
    // same convention as the C++ writer.
    assert!(rust.contains("p1: d2c::jni_sys::jint"), "{rust}");
    assert!(rust.contains("p2: d2c::jni_sys::jint"), "{rust}");
}

#[test]
fn conditional_gotos_and_phi_lowering() {
    let rust = compile(&class(), "max");
    // Both branch targets become explicit state transitions.
    assert!(rust.contains("continue 'd2c;"), "{rust}");
    // `:done` is reached from two predecessors; after phi removal the merged
    // value is the register itself, assigned on both paths.
    assert!(rust.contains("v2 = v0;"), "{rust}");
    assert!(rust.contains("v2 = v1;"), "{rust}");
}

#[test]
fn loops_keep_their_back_edge() {
    let rust = compile(&class(), "sum");
    assert!(
        rust.matches("continue 'd2c;").count() >= 3,
        "loop header + body + exit: {rust}"
    );
    assert!(rust.contains("if (v2 >= v0)"), "{rust}");
    assert!(rust.contains("v1 = (v1 + v2);"), "{rust}");
    assert!(rust.contains("v2 = (v2 + 1);"), "{rust}");
}

#[test]
fn string_constants_are_obfuscated() {
    let rust = compile(&class(), "greet");
    assert!(rust.contains("d2c::new_string_utf(env,"), "{rust}");
    let obfuscated = obfuscated_greet();
    assert!(
        obfuscated.contains("d2c::new_string_utf_obf_seeded(env, "),
        "{obfuscated}"
    );
}

/// The `greet` method of the fixture, compiled with `--obfuscate`.
fn obfuscated_greet() -> String {
    let source = class();
    let method = source.methods.iter().find(|m| m.name == "greet").unwrap();
    let input = decode_method(&source, method, true, false).unwrap();
    IrBuilder::new(&input)
        .process()
        .unwrap()
        .get_source()
        .unwrap()
}

/// The `0x........` literal passed after the byte literal in `rust`.
fn seed_after(rust: &str, needle: &str) -> u32 {
    let (_, after) = byte_literal_after(rust, needle);
    let hex = after
        .trim_start_matches([',', ' '])
        .split([')', ','])
        .next()
        .expect("a seed argument")
        .trim();
    u32::from_str_radix(hex.trim_start_matches("0x"), 16)
        .unwrap_or_else(|_| panic!("not a hex seed: {hex:?} in {after:.40}"))
}

/// `--obfuscate` has to keep the *plaintext* out of the crate, and the ciphertext
/// it writes has to be exactly what the runtime undoes.
///
/// The first half is a bug that shipped: emitting
/// `d2c::new_string_utf_obf(env, &d2c::obfuscate(b"...", d2c::OBFUSCATE_KEY))`
/// does fold at compile time, but rustc still materialises the `b"..."` argument
/// as its own anonymous constant and fat LTO does not drop it -- `strings` read
/// 695 of the 1976 obfuscated constants of a real app straight out of its
/// `libstub.so`. So this asserts the plaintext is gone from the source, not
/// merely that it is wrapped in a call.
///
/// The second half is what stops the generator's copy of the keystream from
/// drifting away from the runtime's: the runtime's own `keystream` has to
/// reproduce the literal byte for byte.
#[test]
fn obfuscated_literals_use_the_runtime_keystream() {
    let rust = obfuscated_greet();
    // The fixture's greeting is `hello`, NUL-terminated for `NewStringUTF`.
    assert!(
        !rust.contains("hello"),
        "the plaintext survived into the generated crate:\n{rust}"
    );
    // The fixed key is gone from the output: one known key would let a single
    // sweep of `.rodata` recover every string in the library.
    assert!(!rust.contains("OBFUSCATE_KEY"), "{rust}");

    let needle = "d2c::new_string_utf_obf_seeded(env, ";
    let cipher = literal_after(&rust, needle);
    let seed = seed_after(&rust, needle);
    let plain = b"hello\x00";

    let runtime: [u8; 6] = dex2rs_runtime::keystream(seed);
    let decoded: Vec<u8> = cipher.iter().zip(runtime).map(|(c, k)| c ^ k).collect();
    assert_eq!(
        decoded,
        plain.to_vec(),
        "the generator's keystream and the runtime's keystream disagree"
    );
    // A no-op keystream would satisfy the comparison above if both sides were
    // wrong the same way, so pin that the ciphertext is not the plaintext.
    assert_ne!(cipher, plain.to_vec());
    assert_eq!(seed, dex2rs_core::codegen::keystream_seed(plain));
}

/// Every constant gets its own keystream, so decoding one string does not decode
/// the next.
#[test]
fn each_string_gets_its_own_seed() {
    let source = class();
    let mut seeds = Vec::new();
    for name in ["greet", "escaped"] {
        let method = source.methods.iter().find(|m| m.name == name).unwrap();
        let input = decode_method(&source, method, true, false).unwrap();
        let rust = IrBuilder::new(&input)
            .process()
            .unwrap()
            .get_source()
            .unwrap();
        seeds.push(seed_after(&rust, "d2c::new_string_utf_obf_seeded(env, "));
    }
    assert_ne!(seeds[0], seeds[1], "two constants share a keystream");
    // ...and the ciphertexts differ even though the two strings have the same
    // length, which a fixed key would not give us.
    assert_ne!(
        dex2rs_core::codegen::obfuscate_bytes(b"hello\x00").1,
        dex2rs_core::codegen::obfuscate_bytes(b"other\x00").1
    );
}

/// `(ciphertext, seed)` for every `d2c::Name::obf(...)` in `rust`.
fn name_sites(rust: &str) -> Vec<(Vec<u8>, u32)> {
    let mut out = Vec::new();
    let mut rest = rust;
    while let Some(pos) = rest.find("d2c::Name::obf(") {
        let tail = &rest[pos..];
        let (body, after) = byte_literal_after(tail, "d2c::Name::obf(");
        out.push((
            decode_byte_literal(&format!("b\"{body}\"")),
            seed_after(tail, "d2c::Name::obf("),
        ));
        rest = after;
    }
    out
}

/// Every byte literal in `rust`, decoded.
fn all_literals(rust: &str) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut rest = rust;
    while let Some(pos) = rest.find("b\"") {
        let tail = &rest[pos..];
        let (body, after) = byte_literal_after(tail, "");
        out.push(decode_byte_literal(&format!("b\"{body}\"")));
        rest = after;
    }
    out
}

/// Decode one obfuscated name with the **runtime's** keystream -- the generator's
/// copy is a separate implementation, and this is the one the device runs.
fn decode_name(cipher: &[u8], seed: u32) -> Vec<u8> {
    // The runtime's `keystream` is const-generic, so take a fixed ceiling and use
    // the prefix; fixture names are far shorter than this.
    const KEYS: usize = 256;
    assert!(
        cipher.len() <= KEYS,
        "fixture name longer than the test buffer"
    );
    let keys: [u8; KEYS] = dex2rs_runtime::keystream(seed);
    cipher.iter().zip(keys).map(|(c, k)| c ^ k).collect()
}

/// Class/field/method names are the other half of what `--obfuscate` has to keep
/// out of the crate.
///
/// They are what the reflection path hands to `FindClass`/`GetMethodID`, and a
/// real app has 40 530 call sites carrying them, so `strings` on `libstub.so`
/// read them straight out. The rule is the one a string constant already has: the
/// plaintext must not exist in the generated crate, and the ciphertext has to be
/// exactly what the runtime's own keystream undoes.
#[test]
fn obfuscated_names_use_the_runtime_keystream() {
    let source = class();
    // One method per kind of name: a field, an invoke, a `const-class` and a
    // `check-cast`.
    for name in ["bump", "callObject", "kind", "check"] {
        let method = source
            .methods
            .iter()
            .find(|m| m.name == name)
            .unwrap_or_else(|| panic!("fixture has a method named {name}"));
        let plain_input = decode_method(&source, method, false, false).unwrap();
        let plain = IrBuilder::new(&plain_input)
            .process()
            .unwrap()
            .get_source()
            .unwrap();
        let obf_input = decode_method(&source, method, true, false).unwrap();
        let obf = IrBuilder::new(&obf_input)
            .process()
            .unwrap()
            .get_source()
            .unwrap();

        let sites = name_sites(&obf);
        assert!(
            !sites.is_empty(),
            "{name} has no obfuscated name in:\n{obf}"
        );
        let plain_literals = all_literals(&plain);
        let obf_literals = all_literals(&obf);
        for (cipher, seed) in sites {
            let decoded = decode_name(&cipher, seed);
            assert!(
                plain_literals.contains(&decoded),
                "{name}: {} decodes to {:?}, which the plain build never passes",
                String::from_utf8_lossy(&cipher),
                String::from_utf8_lossy(&decoded)
            );
            assert!(
                !obf_literals.contains(&decoded),
                "{name}: the name {:?} is still a literal in the obfuscated crate",
                String::from_utf8_lossy(&decoded)
            );
            assert_ne!(
                cipher, decoded,
                "{name}: an obfuscated name equals its plaintext"
            );
        }
    }
}

/// The generator's name bound and the runtime's decode buffer have to agree, and
/// nothing in the type system ties them together.
#[test]
fn name_bound_matches_the_runtime() {
    assert_eq!(
        dex2rs_core::codegen::MAX_OBF_NAME,
        dex2rs_runtime::MAX_NAME,
        "the generator would emit names the runtime cannot decode"
    );
}

#[test]
fn long_arithmetic_uses_wide_slots() {
    // Reference: `jlong v0; jlong v1; v0 = (jlong)p2; v1 = 2; v1 = (v0 * v1);`
    let rust = compile(&class(), "wide");
    assert!(rust.contains("d2c::jni_sys::jlong"), "{rust}");
    assert!(rust.contains("v1 = (v0 * v1);"), "{rust}");
    assert!(
        rust.contains("d2c::jni_sys::jlong {"),
        "the wide return type: {rust}"
    );
}

#[test]
fn divide_by_zero_throws() {
    let rust = compile(&class(), "divZero");
    // The checked operation throws, and the landing pad dispatches on the type.
    assert!(
        rust.contains(
            "d2c::throw_exception(env, b\"java/lang/ArithmeticException\", b\"divide by zero\")"
        ),
        "{rust}"
    );
    assert!(rust.contains("v2 = (v0 / v1);"), "{rust}");
    assert!(rust.contains("d2c::get_pending_exception(env)"), "{rust}");
    assert!(
        rust.contains(
            "d2c::is_instance_of_name(env, exception, b\"java/lang/ArithmeticException\")"
        ),
        "{rust}"
    );
}

#[test]
fn arrays_and_payloads() {
    // Reference: `v1 = (jarray) env->NewIntArray((jint) v0);` and
    // `env->SetIntArrayRegion((jintArray) v1, (jint) v2, 1, &val);` with `val = v0`.
    let rust = compile(&class(), "makeArray");
    assert!(
        rust.contains("v1 = d2c::NewIntArray(env, v0 as d2c::jni_sys::jsize);"),
        "{rust}"
    );
    assert!(
        rust.contains("d2c::SetIntArrayRegion(env, v1, v2 as d2c::jni_sys::jsize, v0);"),
        "{rust}"
    );
    // A negative size must throw before the array is created.
    assert!(
        rust.contains("java/lang/NegativeArraySizeException"),
        "{rust}"
    );

    let rust = compile(&class(), "dataArray");
    assert!(
        rust.contains("d2c::set_int_array(env, v1, &[1, 2, 3]);"),
        "{rust}"
    );
}

#[test]
fn static_fields_are_cached() {
    let rust = compile(&class(), "bump");
    assert!(
        rust.contains("d2c::resolve_field(env, &mut cls0, &mut fld0, true,"),
        "{rust}"
    );
    assert!(
        rust.contains("d2c::GetStaticIntField(env, cls0, fld0)"),
        "{rust}"
    );
    assert!(
        rust.contains("d2c::SetStaticIntField(env, cls0, fld0,"),
        "{rust}"
    );
}

#[test]
fn virtual_calls_use_the_jvalue_array() {
    let rust = compile(&class(), "callObject");
    // Object parameters are stored as a fresh local reference, like the C++
    // writer's `v0 = (jobject)env->NewLocalRef(p1);`.
    assert!(rust.contains("v0 = d2c::new_local_ref(env, p1);"), "{rust}");
    assert!(
        rust.contains("d2c::resolve_method(env, &mut cls0, &mut mth0, false,"),
        "{rust}"
    );
    assert!(
        rust.contains("d2c::call_int_method(env, d2c::CALL_VIRTUAL,"),
        "{rust}"
    );
    assert!(rust.contains("__args.as_ptr()"), "{rust}");
    // `move-result` reads the call result out of its own register.
    assert!(rust.contains("v2 = v1;"), "{rust}");
}

/// An invoke whose result is never used is a dead temporary. The reference frees
/// it *after* the call, once its slot holds the returned reference; freeing it
/// before the call frees whatever handle the slot still aliases, which is how a
/// real APK crashed on device (`SIGSEGV` in `ui/components/smali/n;->invoke`,
/// where `v4 = v3` keeps the field value alive in another register while the
/// dead `Object.getClass` result reuses `v3`).
#[test]
fn dead_invoke_result_is_deleted_after_the_call() {
    let rust = compile(&class(), "deadResult");
    let call = rust
        .find("d2c::call_object_method(")
        .unwrap_or_else(|| panic!("the call is emitted:\n{rust}"));
    assert!(
        !rust[..call].contains("delete_local_ref"),
        "the dead result's slot must not be freed before the call:\n{rust}"
    );
    assert!(
        rust[call..].contains("d2c::delete_local_ref(env, v"),
        "the dead result must still be released after the call:\n{rust}"
    );
}

#[test]
fn instanceof_and_const_class() {
    let rust = compile(&class(), "check");
    assert!(
        rust.contains("d2c::is_instance_of(env, v0, cls0)"),
        "{rust}"
    );

    let rust = compile(&class(), "kind");
    // `const-class` resolves the class and returns a new local reference.
    assert!(
        rust.contains("d2c::resolve_class(env, &mut cls0, b\"java/lang/String\")"),
        "{rust}"
    );
    assert!(
        rust.contains("v0 = d2c::new_local_ref(env, cls0);"),
        "{rust}"
    );
}

/// Cache slots are numbered per family, exactly like writer.py's three
/// `TmpnameAllocator`s: `cls0` and `fld0`/`mth0` coexist in one method.
#[test]
fn cache_slots_are_numbered_per_family() {
    let rust = compile(&class(), "bump");
    assert!(rust.contains("let mut cls0:"), "{rust}");
    assert!(rust.contains("let mut fld0:"), "{rust}");

    let rust = compile(&class(), "callObject");
    assert!(rust.contains("let mut cls0:"), "{rust}");
    assert!(rust.contains("let mut mth0:"), "{rust}");
}

/// The dex must round-trip through smali and still compile: this is the path the
/// CLI uses (`DexFile::from_bytes` -> `to_smali` -> compile -> `from_smali`).
#[test]
fn dex_round_trip() {
    let source = class();
    let dex = DexFile::from_smali(&[source]).expect("assemble");
    let bytes = dex.to_bytes().to_vec();
    let parsed = DexFile::from_bytes(&bytes).expect("parse");
    let classes = parsed.to_smali().expect("disassemble");
    assert_eq!(classes.len(), 1);

    let mut compiled = 0;
    for method in &classes[0].methods {
        let input = decode_method(&classes[0], method, false, false).expect("decode");
        let irmethod = IrBuilder::new(&input)
            .process()
            .unwrap_or_else(|e| panic!("{}: {e}", method.name));
        irmethod.get_source().expect("codegen");
        compiled += 1;
    }
    assert_eq!(compiled, classes[0].methods.len());
}

/// A register reused for a different type *after* the last instruction in a
/// `try` range that can throw must not leak that write into the handler.
///
/// Control leaves the range at the throwing instruction, so a `monitor-exit`
/// there sees the monitor object -- not whatever the range happens to hold at
/// its end. Reading the block-end state instead typed the handler's phi as a
/// `long` and made the method unconvertible; it also produced genuinely wrong
/// code wherever an array element type was derived from the same state.
#[test]
fn a_write_after_the_last_throw_does_not_reach_the_handler() {
    let rust = compile(&class(), "reuseAfterThrow");

    // The monitor taken by `monitor-enter` is the one both `monitor-exit`
    // calls (the protected one and the handler's) must release.
    let needle = "d2c::monitor_enter(env, ";
    let start = rust.find(needle).expect("monitor-enter") + needle.len();
    let rest = &rust[start..];
    let var = &rest[..rest.find(')').expect("monitor-enter argument")];

    let exits = rust
        .matches(&format!("d2c::monitor_exit(env, {var})"))
        .count();
    assert_eq!(
        exits, 2,
        "both monitor-exit calls must release {var}:\n{rust}"
    );
}

/// `can_throw` is what decides whether a block may hand control to a handler,
/// so an op that cannot raise must not be treated as one. The integral
/// `div-`/`rem-` forms are the exception, and are checked because the tempting
/// grouping is by first letter.
#[test]
fn only_instructions_that_raise_can_reach_a_handler() {
    use dex2rs_core::graph::can_throw;

    for safe in [
        "nop",
        "move-object/from16",
        "move-result",
        "const-string",
        "int-to-long",
        "add-long/2addr",
        "ushr-int/lit8",
        "cmpg-float",
        "if-eqz",
        "goto/32",
        "return-void",
        "instance-of",
        "packed-switch",
    ] {
        assert!(!can_throw(safe), "{safe} cannot throw");
    }

    for throwing in [
        "invoke-virtual/range",
        "iget-object",
        "aput-wide",
        "monitor-exit",
        "check-cast",
        "new-instance",
        "array-length",
        "throw",
        "div-int/lit8",
        "rem-long",
        "filled-new-array",
        "sget-object",
        "some-future-opcode",
    ] {
        assert!(can_throw(throwing), "{throwing} can throw");
    }
}

/// `invoke-polymorphic` is lowered to one runtime call, and the call-site
/// prototype (not the erased JNI descriptor) decides the boxing, the unboxing
/// and the target kind.
///
/// This instruction is what d8 emits for `MethodHandle.invoke`/`invokeExact`
/// and every `VarHandle` access mode; before it was lowered, a method holding
/// one could not be converted at all.
#[test]
fn polymorphic_invoke_is_one_runtime_call() {
    let source = polymorphic_class();

    // `(II)I` on a MethodHandle: box both ints, unbox the int result.
    let ints = compile(&source, "handleCall");
    assert!(
        ints.contains(
            "d2c::unbox_int(env, d2c::invoke_polymorphic(env, v0, d2c::HANDLE_METHOD, \
             b\"invokeExact\", b\"(II)I\", __args.as_ptr()))"
        ),
        "{ints}"
    );

    // The `/range` form must produce the same call.
    let ranged = compile(&source, "handleRange");
    assert!(ranged.contains("b\"(II)I\""), "{ranged}");
    assert!(ranged.contains("d2c::HANDLE_METHOD"), "{ranged}");

    // A `VarHandle` access mode is named exactly as the dex names it, and an
    // array argument is *not* boxed (`jv_object`, as any reference argument).
    let var = compile(&source, "varHandleCall");
    assert!(
        var.contains("d2c::HANDLE_VAR, b\"getAndAdd\", b\"([III)I\""),
        "{var}"
    );
    assert!(var.contains("d2c::jv_object("), "{var}");

    // A void call site drops the result: no unbox, no assignment.
    let void = compile(&source, "varHandleVoid");
    assert!(void.contains("b\"([II)V\""), "{void}");
    assert!(
        !void.contains("unbox_"),
        "a void call site unboxed something: {void}"
    );

    // A reference result is already an object.
    let object = compile(&source, "handleObject");
    assert!(
        object.contains("b\"(Ljava/lang/String;)Ljava/lang/String;\""),
        "{object}"
    );
    assert!(
        !object.contains("unbox_"),
        "a reference result was unboxed: {object}"
    );

    // A wide argument passes both of its registers; the second one must not be
    // pushed as an extra argument.
    let wide = compile(&source, "handleWide");
    assert!(wide.contains("b\"(JII)J\""), "{wide}");
    assert!(
        wide.contains("let __args: [d2c::jni_sys::jvalue; 3]"),
        "a `(JII)J` call site has 3 arguments: {wide}"
    );
    assert!(wide.contains("d2c::jv_long("), "{wide}");
}

/// `const-method-type` and `const-method-handle` are one runtime call each.
///
/// Neither has a JNI equivalent (`MethodType` and `MethodHandle` are ordinary
/// classes, but a handle's *kind* is a dex concept), so the emitter passes the
/// kind to the runtime as the runtime's own constant and lets
/// `MethodHandles.Lookup` resolve the member.
#[test]
fn invoke_constants_are_runtime_calls() {
    let source = invoke_const_class();

    // `const-method-type v0, (ILjava/lang/String;)V`
    let method_type = compile(&source, "methodTypeConstant");
    assert!(
        method_type.contains("d2c::method_type(env, b\"(ILjava/lang/String;)V\")"),
        "{method_type}"
    );
    assert!(
        method_type.contains("as d2c::jni_sys::jobject"),
        "{method_type}"
    );

    // A `void` return type has to reach `Void.TYPE`, not a lookup failure.
    assert!(compile(&source, "voidMethodType").contains("b\"()V\""));

    // Each handle kind maps to its own runtime constant, and the field kinds
    // carry a field descriptor while the method kinds carry a prototype.
    for (method, constant, desc) in [
        ("staticGetter", "d2c::HANDLE_KIND_STATIC_GET", "b\"I\""),
        ("staticSetter", "d2c::HANDLE_KIND_STATIC_PUT", "b\"I\""),
        ("instanceGetter", "d2c::HANDLE_KIND_INSTANCE_GET", "b\"I\""),
        ("instanceSetter", "d2c::HANDLE_KIND_INSTANCE_PUT", "b\"I\""),
        (
            "staticMethodHandle",
            "d2c::HANDLE_KIND_INVOKE_STATIC",
            "b\"(I)I\"",
        ),
        (
            "virtualMethodHandle",
            "d2c::HANDLE_KIND_INVOKE_INSTANCE",
            "b\"()I\"",
        ),
        (
            "interfaceMethodHandle",
            "d2c::HANDLE_KIND_INVOKE_INTERFACE",
            "b\"()V\"",
        ),
        (
            "constructorHandle",
            "d2c::HANDLE_KIND_INVOKE_CONSTRUCTOR",
            "b\"()V\"",
        ),
        (
            "directMethodHandle",
            "d2c::HANDLE_KIND_INVOKE_DIRECT",
            "b\"()V\"",
        ),
    ] {
        let rust = compile(&source, method);
        assert!(
            rust.contains("d2c::method_handle(env, "),
            "{method}: {rust}"
        );
        assert!(rust.contains(constant), "{method}: {rust}");
        assert!(rust.contains(desc), "{method} should keep {desc}: {rust}");
    }
}

/// The kind is part of the handle literal, so it has to survive smali -> dex ->
/// smali. If it were dropped, a `static-put` and a `static-get` of one field
/// would become indistinguishable and the lowering would have to refuse both.
#[test]
fn method_handle_kind_survives_the_dex_round_trip() {
    let source = invoke_const_class();
    let dex = DexFile::from_smali(std::slice::from_ref(&source)).expect("assemble");
    let bytes = dex.to_bytes().to_vec();
    let text = DexFile::from_bytes(&bytes)
        .expect("re-read")
        .to_smali()
        .expect("disassemble")[0]
        .to_smali();

    for literal in [
        "static-get@Lcom/example/InvokeConst;->s:I",
        "static-put@Lcom/example/InvokeConst;->s:I",
        "instance-get@Lcom/example/InvokeConst;->instance:I",
        "instance-put@Lcom/example/InvokeConst;->instance:I",
        "invoke-static@Ljava/lang/Math;->abs(I)I",
        "invoke-instance@Ljava/lang/String;->length()I",
        "invoke-interface@Ljava/lang/Runnable;->run()V",
        "invoke-constructor@Ljava/lang/StringBuilder;-><init>()V",
    ] {
        assert!(text.contains(literal), "round trip lost {literal}:\n{text}");
    }
    assert!(
        text.contains("const-method-type v0, (ILjava/lang/String;)V"),
        "{text}"
    );
}

#[test]
#[ignore]
fn dump_all_sources() {
    let source = class();
    for method in &source.methods {
        println!("===== {} =====", method.name);
        println!("{}", compile(&source, &method.name));
    }
}
