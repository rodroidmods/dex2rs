//! Port of `dex2c/util.py`.
//!
//! Type descriptors are kept as plain `String`s ('I', 'J', 'Ljava/lang/String;',
//! '[I', ...), exactly like the Python implementation, so the type-inference
//! rules can be ported line by line.

/// `TYPE_DESCRIPTOR` from util.py: primitive descriptor -> JNI call suffix.
pub fn type_descriptor(atype: &str) -> &'static str {
    match atype {
        "V" => "Void",
        "Z" => "Boolean",
        "B" => "Byte",
        "S" => "Short",
        "C" => "Char",
        "I" => "Int",
        "J" => "Long",
        "F" => "Float",
        "D" => "Double",
        _ => "Object",
    }
}

/// `DECL_JNI_TYPE` from util.py: descriptor -> C/JNI type name.
/// Returns `None` for reference types (see [`get_native_type`]).
fn decl_jni_type(jtype: &str) -> Option<&'static str> {
    match jtype {
        "Z" => Some("jboolean"),
        "B" => Some("jbyte"),
        "S" => Some("jshort"),
        "C" => Some("jchar"),
        "I" => Some("jint"),
        "J" => Some("jlong"),
        "F" => Some("jfloat"),
        "D" => Some("jdouble"),
        "V" => Some("void"),
        _ => None,
    }
}

fn primitive_type_order(atype: &str) -> Option<u32> {
    match atype {
        "Z" => Some(1),
        "B" => Some(2),
        "S" => Some(3),
        "C" => Some(3),
        "I" => Some(5),
        "J" => Some(6),
        "F" => Some(8),
        "D" => Some(9),
        _ => None,
    }
}

/// `get_native_type()`: descriptor -> JNI host type (`jint`, `jstring`, `jobject`, `jarray`).
pub fn get_native_type(jtype: &str) -> String {
    if let Some(t) = decl_jni_type(jtype) {
        return t.to_string();
    }
    if let Some(rest) = jtype.strip_prefix('L') {
        let _ = rest;
        if jtype == "Ljava/lang/String;" {
            return "jstring".to_string();
        }
        return "jobject".to_string();
    }
    if jtype.starts_with('[') {
        return "jarray".to_string();
    }
    "jobject".to_string()
}

/// `get_type()`: strip an object descriptor down to its JNI path
/// (`Ljava/lang/String;` -> `java/lang/String`), spilling primitives to their
/// Java names; arrays pass through unchanged.
pub fn get_type(atype: &str) -> String {
    match atype {
        "V" => "void".to_string(),
        "Z" => "boolean".to_string(),
        "B" => "byte".to_string(),
        "S" => "short".to_string(),
        "C" => "char".to_string(),
        "I" => "int".to_string(),
        "J" => "long".to_string(),
        "F" => "float".to_string(),
        "D" => "double".to_string(),
        _ => {
            let bytes = atype.as_bytes();
            if bytes.first() == Some(&b'L') && bytes.len() >= 2 && bytes[bytes.len() - 1] == b';' {
                atype[1..atype.len() - 1].to_string()
            } else {
                atype.to_string()
            }
        }
    }
}

/// `get_fully_qualified_class_name()`: like [`get_type`] but only for object descriptors.
pub fn get_fully_qualified_class_name(signature: &str) -> String {
    if let Some(stripped) = signature
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
    {
        stripped.to_string()
    } else {
        signature.to_string()
    }
}

/// `get_cdecl_type()`: descriptor -> C/C++ local variable type used by the generated code.
pub fn get_cdecl_type(atype: &str) -> &'static str {
    match atype {
        "Z" | "B" | "C" | "S" | "I" => "jint",
        "J" => "jlong",
        "F" => "jfloat",
        "D" => "jdouble",
        _ => "jobject",
    }
}

pub fn is_primitive_type(atype: &str) -> bool {
    primitive_type_order(atype).is_some()
}

/// `get_params_type()`: split a method descriptor's parameter list.
pub fn get_params_type(descriptor: &str) -> Vec<String> {
    let Some(close) = descriptor.find(')') else {
        return Vec::new();
    };
    let params = &descriptor[1..close];
    let mut out = Vec::new();
    let mut chars = params.chars().peekable();
    while let Some(c) = chars.next() {
        // androguard renders prototypes with spaces between descriptors; the
        // smali crate gives us the compact JNI form. Accept both.
        if c.is_whitespace() {
            continue;
        }
        match c {
            '[' => {
                // An array is '[' repetitions followed by the element descriptor.
                let mut arr = String::from("[");
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next == '[' {
                        arr.push('[');
                    } else {
                        arr.push_str(&consume_type(next, &mut chars));
                        break;
                    }
                }
                out.push(arr);
            }
            other => out.push(consume_type(other, &mut chars)),
        }
    }
    out
}

/// Consume one type descriptor starting with `first` (whose remaining chars come
/// from `chars`); object types run until ';'.
fn consume_type(first: char, chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut out = String::new();
    out.push(first);
    if first == 'L' {
        for c in chars.by_ref() {
            out.push(c);
            if c == ';' {
                break;
            }
        }
    }
    out
}

/// `get_type_size()`: number of registers occupied by a parameter.
pub fn get_type_size(param: &str) -> i64 {
    match param {
        "J" | "D" => 2,
        _ => 1,
    }
}

pub fn is_int(atype: &str) -> bool {
    atype.len() == 1 && "ZBCSIJ".contains(atype)
}

pub fn is_long(atype: &str) -> bool {
    atype == "J"
}

pub fn is_float(atype: &str) -> bool {
    atype.len() == 1 && "FD".contains(atype)
}

pub fn is_ref(atype: &str) -> bool {
    atype.starts_with('L') || atype.starts_with('[')
}

pub fn is_array(atype: &str) -> bool {
    atype.starts_with('[')
}

pub fn is_java_lang_object(atype: &str) -> bool {
    atype == "Ljava/lang/Object;"
}

pub fn is_java_lang_object_array(atype: &str) -> bool {
    is_array(atype) && atype.ends_with("Ljava/lang/Object;")
}

/// `compare_primitive_type()`: `None` sorts below any type; otherwise compare widths.
pub fn compare_primitive_type(type1: Option<&str>, type2: Option<&str>) -> i32 {
    match (type1, type2) {
        (None, None) => 0,
        (None, Some(_)) => -1,
        (Some(_), None) => 1,
        (Some(t1), Some(t2)) => {
            let o1 = primitive_type_order(t1).unwrap_or(0) as i32;
            let o2 = primitive_type_order(t2).unwrap_or(0) as i32;
            o1 - o2
        }
    }
}

pub fn get_smaller_type(type1: &str, type2: &str) -> String {
    if compare_primitive_type(Some(type1), Some(type2)) < 0 {
        type1.to_string()
    } else {
        type2.to_string()
    }
}

pub fn get_bigger_type(type1: &str, type2: &str) -> String {
    if compare_primitive_type(Some(type1), Some(type2)) > 0 {
        type1.to_string()
    } else {
        type2.to_string()
    }
}

/// `merge_array_type()` -- with one deliberate difference from the reference.
///
/// `java.lang.Object` is the least upper bound of every array type, so the
/// reference returns it whenever an array meets `Object`. That is correct as a
/// Java type but lossy as a *compiler* type: the element type of an array access
/// can only come from the array's own descriptor, so the moment a register's
/// type collapses to `Object` every `aget`/`aput` through it has no element type
/// to infer, the destination value never gets one, and the method fails with
/// "unknown type" instead of converting.
///
/// Keeping the array type instead costs nothing and is strictly more precise:
/// every array type is a reference for codegen (`jobject`), so nothing that
/// treats the value as an `Object` can tell the difference, while `aget` can now
/// pick the right `Get<Type>ArrayRegion`. And a register that reaches an
/// `aget` or `aput` is an array on every path -- the verifier guarantees it, or
/// the dex would not have loaded -- so preferring the array cannot describe the
/// value wrongly. On the reference app this is the difference between 8 methods
/// failing to convert and none.
fn merge_array_type(type1: &str, type2: &str) -> Option<String> {
    if is_java_lang_object(type2) {
        return Some(if is_array(type1) {
            type1.to_string()
        } else {
            type2.to_string()
        });
    }
    if is_java_lang_object(type1) {
        return Some(if is_array(type2) {
            type2.to_string()
        } else {
            type1.to_string()
        });
    }
    if is_array(type1) {
        if is_array(type2) {
            let new_type = merge_type(Some(&type1[1..]), Some(&type2[1..]));
            new_type.map(|t| format!("[{t}"))
        } else {
            Some("Ljava/lang/Object;".to_string())
        }
    } else {
        merge_array_type(type2, type1)
    }
}

/// True for the primitive value kinds whose representation is not a reference
/// (`is_int` or `is_float`, i.e. not `is_ref`, not `is_array`, not `V`).
///
/// Used to detect a phi that merges *incompatible kinds* -- a register reused
/// for a `long` on one path and an object on another. The DEX verifier allows
/// that (it types per program point) but a single SSA value cannot express it.
pub fn is_scalar(atype: &str) -> bool {
    is_int(atype) || is_float(atype)
}

/// `merge_reference_type()`
fn merge_reference_type(type1: &str, type2: &str) -> String {
    if type1 == type2 {
        type1.to_string()
    } else if is_java_lang_object(type1) || is_java_lang_object(type2) {
        "Ljava/lang/Object;".to_string()
    } else {
        "Ljava/lang/Object;".to_string()
    }
}

/// `merge_type()`: the least upper bound of two descriptors (`None` when unrelated).
pub fn merge_type(type1: Option<&str>, type2: Option<&str>) -> Option<String> {
    let (Some(t1), Some(t2)) = (type1, type2) else {
        return type1
            .map(|s| s.to_string())
            .or_else(|| type2.map(|s| s.to_string()));
    };

    if (is_int(t1) && is_int(t2)) || (is_float(t1) && is_float(t2)) {
        Some(get_bigger_type(t1, t2))
    } else if is_array(t1) || is_array(t2) {
        match merge_array_type(t1, t2) {
            None => Some("Ljava/lang/Object;".to_string()),
            other => other,
        }
    } else if is_ref(t1) || is_ref(t2) {
        Some(merge_reference_type(t1, t2))
    } else {
        None
    }
}

/// `MangleForJni()` from art/runtime/utils.cc.
pub fn mangle_for_jni(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            result.push(ch);
        } else {
            match ch {
                '.' | '/' => result.push('_'),
                '_' => result.push_str("_1"),
                ';' => result.push_str("_2"),
                '[' => result.push_str("_3"),
                _ => result.push_str(&format!("_0{:04x}", ch as u32)),
            }
        }
    }
    result
}

/// `JniShortName()`: `Java_<mangled class>_<mangled method>`.
pub fn jni_short_name(cls_name: &str, method_name: &str) -> String {
    debug_assert!(cls_name.starts_with('L'));
    debug_assert!(cls_name.ends_with(';'));
    let cls = &cls_name[1..cls_name.len() - 1];
    format!(
        "Java_{}_{}",
        mangle_for_jni(cls),
        mangle_for_jni(method_name)
    )
}

/// `JniLongName()`: short name plus the mangled parameter list (ART's long form).
pub fn jni_long_name(cls_name: &str, method_name: &str, signature: &str) -> String {
    let mut long_name = jni_short_name(cls_name, method_name);
    long_name.push_str("__");
    let signature = &signature[1..];
    let index = signature.find(')').unwrap_or(signature.len());
    long_name.push_str(&mangle_for_jni(&signature[..index]));
    long_name
}

/// `get_method_triple()`: `(class, name, proto)` with the return type kept or
/// stripped (`proto` becomes `(args)` when `return_type` is false).
pub fn method_triple(
    cls_name: &str,
    name: &str,
    proto: &str,
    return_type: bool,
) -> (String, String, String) {
    if return_type {
        (cls_name.to_string(), name.to_string(), proto.to_string())
    } else {
        let index = proto.find(')').map(|i| i + 1).unwrap_or(proto.len());
        (
            cls_name.to_string(),
            name.to_string(),
            proto[..index].to_string(),
        )
    }
}

/// `hex_escape_string()`: every byte as `\xNN` (used for C string literals and traces).
pub fn hex_escape_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len() * 4);
    for &b in s.as_bytes() {
        result.push_str(&format!("\\x{b:02x}"));
    }
    result
}

/// Re-encode a Rust string as JNI's *modified* UTF-8 for `NewStringUTF`.
///
/// `NewStringUTF` does not take plain UTF-8: the JNI specification defines the
/// accepted encoding as modified UTF-8, where U+0000 is `C0 80` and a
/// supplementary character is a CESU-8 surrogate pair rather than the four-byte
/// UTF-8 form.
///
/// Why this matters: passing raw UTF-8 for a non-BMP character (an emoji, say)
/// is decoded differently by different VMs -- a desktop JVM reads the four-byte
/// sequence as Latin-1 garbage (`\xf0\x9f\x90\x9b` came back as "\u00f0"), while
/// ART happens to accept it. That leniency is what the C++ writer relied on, so
/// the same app would render the string on a phone and corrupt it everywhere
/// else. Emitting the encoding the API documents removes the guesswork; ART
/// accepts modified UTF-8 natively, and the host JVM probe asserts the round
/// trip.
///
/// The result is *not* NUL terminated -- callers append the terminator (the
/// emitter's string literal does, so the obfuscation path can XOR it too).
pub fn mutf8(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for ch in s.chars() {
        let code = ch as u32;
        match code {
            // Modified UTF-8 spells U+0000 as an overlong two-byte sequence so
            // that the terminator stays unambiguous.
            0 => out.extend_from_slice(&[0xC0, 0x80]),
            0x01..=0x7F => out.push(code as u8),
            0x80..=0x7FF => {
                out.push(0xC0 | (code >> 6) as u8);
                out.push(0x80 | (code & 0x3F) as u8);
            }
            0x800..=0xFFFF => {
                out.push(0xE0 | (code >> 12) as u8);
                out.push(0x80 | ((code >> 6) & 0x3F) as u8);
                out.push(0x80 | (code & 0x3F) as u8);
            }
            _ => {
                // CESU-8: encode as the UTF-16 surrogate pair's two halves.
                let v = code - 0x10000;
                let high = 0xD800 + (v >> 10);
                let low = 0xDC00 + (v & 0x3FF);
                for unit in [high, low] {
                    out.push(0xE0 | (unit >> 12) as u8);
                    out.push(0x80 | ((unit >> 6) & 0x3F) as u8);
                    out.push(0x80 | (unit & 0x3F) as u8);
                }
            }
        }
    }
    out
}

/// Access flags for methods (`ACCESS_FLAGS_METHODS` in util.py).
pub fn get_access_method(access: u32) -> Vec<String> {
    const ORDER: [u32; 16] = [
        0x1, 0x4, 0x2, 0x400, 0x8, 0x10, 0x80, 0x40, 0x20, 0x100, 0x800, 0x200, 0x1000, 0x2000,
        0x4000, 0x10000,
    ];
    let mut out = Vec::new();
    for flag in ORDER {
        if flag & access != 0 {
            let mapped = match flag {
                0x1 => "public",
                0x2 => "private",
                0x4 => "protected",
                0x8 => "static",
                0x10 => "final",
                0x20 => "synchronized",
                0x40 => "bridge",
                0x80 => "varargs",
                0x100 => "native",
                0x400 => "abstract",
                0x800 => "strictfp",
                0x1000 => "synthetic",
                0x4000 => "enum",
                0x10000 => "constructor",
                _ => "unknown",
            };
            out.push(mapped.to_string());
        }
    }
    out
}

/// Access flags used by the CLI for filtering (mirrors the androguard checks).
pub const ACC_STATIC: u32 = 0x8;
pub const ACC_NATIVE: u32 = 0x100;
pub const ACC_ABSTRACT: u32 = 0x400;
pub const ACC_SYNTHETIC: u32 = 0x1000;
pub const ACC_CONSTRUCTOR: u32 = 0x10000;

pub fn is_synthetic(access: u32) -> bool {
    access & ACC_SYNTHETIC != 0
}

pub fn is_native(access: u32) -> bool {
    access & ACC_NATIVE != 0
}

pub fn is_static(access: u32) -> bool {
    access & ACC_STATIC != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Modified UTF-8 is what `NewStringUTF` accepts: ASCII passes through, a
    /// two- and a three-byte character keep their UTF-8 bytes, U+0000 becomes
    /// `C0 80` and a supplementary character becomes a CESU-8 surrogate pair
    /// rather than the four-byte UTF-8 form.
    #[test]
    fn mutf8_encodes_for_new_string_utf() {
        assert_eq!(mutf8("abc"), b"abc");
        assert_eq!(mutf8("\u{7f}"), b"\x7f");
        // U+00E9 and U+20AC stay identical to their UTF-8 encoding.
        assert_eq!(mutf8("\u{e9}"), b"\xc3\xa9");
        assert_eq!(mutf8("\u{20ac}"), b"\xe2\x82\xac");
        // U+0000 must not become a bare NUL byte.
        assert_eq!(mutf8("\u{0}"), b"\xc0\x80");
        // U+1F41B is four UTF-8 bytes but six modified UTF-8 bytes.
        assert_eq!(mutf8("\u{1f41b}"), b"\xed\xa0\xbd\xed\xb0\x9b");
        assert_ne!(mutf8("\u{1f41b}"), "\u{1f41b}".as_bytes());
        // U+D800..U+DFFF are not representable in a Rust `char`, so a string can
        // never carry a lone surrogate through here.
        assert_eq!(mutf8("\u{fffd}"), b"\xef\xbf\xbd");
    }

    #[test]
    fn jni_names_match_art() {
        assert_eq!(
            jni_long_name("Lcom/example/Test;", "doIt", "(Ljava/lang/String;I)V"),
            "Java_com_example_Test_doIt__Ljava_lang_String_2I"
        );
    }

    /// A `$` in a method name is not special-cased: `MangleForJni` escapes it to
    /// `_00024`, and the class/method separator `_` sits in front of it, so the
    /// exported symbol really does contain a double underscore. Getting this
    /// wrong is invisible in the generated source and fatal at first call
    /// (`UnsatisfiedLinkError`), so it is pinned here.
    #[test]
    fn jni_names_escape_dollar() {
        assert_eq!(
            jni_long_name(
                "Lcom/example/ThemeMode;",
                "$values",
                "(I[Ljava/lang/String;)V"
            ),
            "Java_com_example_ThemeMode__00024values__I_3Ljava_lang_String_2"
        );
    }

    #[test]
    fn params_of_descriptor() {
        assert_eq!(
            get_params_type("(ILjava/lang/String;[J[[I)V"),
            vec!["I", "Ljava/lang/String;", "[J", "[[I"]
        );
        assert_eq!(get_params_type("()V"), Vec::<String>::new());
        // androguard's spaced form must parse the same way
        assert_eq!(
            get_params_type("(I Ljava/lang/String;)"),
            vec!["I", "Ljava/lang/String;"]
        );
    }

    #[test]
    fn type_merges() {
        assert_eq!(merge_type(Some("I"), Some("J")), Some("J".to_string()));
        assert_eq!(
            merge_type(Some("Ljava/lang/String;"), Some("Ljava/lang/Object;")),
            Some("Ljava/lang/Object;".to_string())
        );
        assert_eq!(merge_type(Some("[I"), Some("[I")), Some("[I".to_string()));
        assert_eq!(merge_type(Some("[I"), Some("[J")), Some("[J".to_string()));
        assert_eq!(
            merge_type(Some("[I"), Some("Ljava/lang/String;")),
            Some("Ljava/lang/Object;".to_string())
        );
        assert_eq!(merge_type(None, Some("I")), Some("I".to_string()));
    }

    /// The one deliberate difference from the reference in the merge rules.
    ///
    /// Merging an array with `java.lang.Object` keeps the array, because the
    /// element type of an `aget`/`aput` is only recoverable from the array's own
    /// descriptor; collapsing to `Object` there is what made 8 methods of the
    /// reference app fail to convert with "unknown type". Two plain references
    /// still merge to the least upper bound.
    #[test]
    fn merging_an_array_with_object_keeps_the_array() {
        assert_eq!(
            merge_type(Some("[I"), Some("Ljava/lang/Object;")),
            Some("[I".to_string())
        );
        assert_eq!(
            merge_type(Some("Ljava/lang/Object;"), Some("[J")),
            Some("[J".to_string())
        );
        assert_eq!(
            merge_type(Some("[[Ljava/lang/String;"), Some("Ljava/lang/Object;")),
            Some("[[Ljava/lang/String;".to_string())
        );
        // A non-array reference still widens to Object as before.
        assert_eq!(
            merge_type(Some("Ljava/lang/String;"), Some("Ljava/lang/Object;")),
            Some("Ljava/lang/Object;".to_string())
        );
        // Two arrays merge element-wise: an `int[]` and an `int[][]` have
        // `java.lang.Object[]` as their upper bound, so the *element* becomes
        // `Object` while the array-ness survives.
        assert_eq!(
            merge_type(Some("[I"), Some("[[I")),
            Some("[Ljava/lang/Object;".to_string())
        );
    }
}
