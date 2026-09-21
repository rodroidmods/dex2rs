//! Port of `dcc.py`'s `MethodFilter`.
//!
//! The filter file keeps its original syntax:
//!
//! * `#...` — comment,
//! * `!regex` — never compile matching methods (checked first),
//! * `=class->name(proto)` — compile on an exact full-name match,
//! * `regex` — compile when it matches the method's full name
//!   (`class` + `name` + `proto`).
//!
//! On top of that, the same structural rules apply: skip methods the JVM/ART
//! could not bind (conflicting return types, existing natives, and anything an
//! interface declares), skip `<clinit>`, and skip `<init>` unless requested.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use regex::Regex;
use smali::types::{Modifier, SmaliAnnotation, SmaliClass, SmaliMethod};

/// `(class descriptor, method name, prototype)`.
pub type MethodTriple = (String, String, String);

fn is_annotated_with_dex2c(annotations: &[SmaliAnnotation]) -> bool {
    annotations
        .iter()
        .any(|a| a.annotation_type.to_jni().ends_with("Dex2C;"))
}

pub struct MethodFilter {
    compile_filters: Vec<Regex>,
    keep_filters: Vec<Regex>,
    compile_full_match: HashSet<String>,
    conflict_methods: HashSet<MethodTriple>,
    native_methods: HashSet<(String, String)>,
    annotated_methods: HashSet<MethodTriple>,
    skip_synthetic: bool,
    allow_init: bool,
}

impl MethodFilter {
    pub fn new(
        config: &Path,
        classes: &[SmaliClass],
        skip_synthetic: bool,
        allow_init: bool,
    ) -> Result<Self> {
        let mut filter = MethodFilter {
            compile_filters: Vec::new(),
            keep_filters: Vec::new(),
            compile_full_match: HashSet::new(),
            conflict_methods: HashSet::new(),
            native_methods: HashSet::new(),
            annotated_methods: HashSet::new(),
            skip_synthetic,
            allow_init,
        };
        filter.load_rules(config)?;
        filter.scan(classes);
        Ok(filter)
    }

    fn load_rules(&mut self, config: &Path) -> Result<()> {
        if !config.exists() {
            return Ok(());
        }
        let text = std::fs::read_to_string(config)
            .with_context(|| format!("reading filter file {}", config.display()))?;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix('!') {
                self.keep_filters.push(Regex::new(rest.trim())?);
            } else if let Some(rest) = line.strip_prefix('=') {
                self.compile_full_match.insert(rest.trim().to_string());
            } else {
                self.compile_filters.push(Regex::new(line)?);
            }
        }
        Ok(())
    }

    /// dcc.py's `_init_conflict_methods` / `_init_native_methods` /
    /// `_init_annotation_methods`, all in one pass over the classes.
    fn scan(&mut self, classes: &[SmaliClass]) {
        // `(class, name, (args))` -> the first triple seen with that shape.
        let mut seen: std::collections::HashMap<MethodTriple, MethodTriple> =
            std::collections::HashMap::new();
        for class in classes {
            let cls = class.name.as_jni_type();
            let class_annotated = is_annotated_with_dex2c(&class.annotations);
            for method in &class.methods {
                let proto = method.signature.to_jni();
                let triple = (cls.clone(), method.name.clone(), proto.clone());
                // Methods that only differ by return type cannot be exported
                // with the same JNI long name: mark both sides as conflicting.
                let return_less = (cls.clone(), method.name.clone(), strip_return_type(&proto));
                match seen.entry(return_less) {
                    std::collections::hash_map::Entry::Occupied(entry) => {
                        self.conflict_methods.insert(triple.clone());
                        self.conflict_methods.insert(entry.get().clone());
                    }
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(triple.clone());
                    }
                }
                if method.modifiers.contains(&Modifier::Native) {
                    self.native_methods
                        .insert((cls.clone(), method.name.clone()));
                }
                if class_annotated || is_annotated_with_dex2c(&method.annotations) {
                    let synthetic = method.modifiers.contains(&Modifier::Synthetic);
                    let native = method.modifiers.contains(&Modifier::Native);
                    if !synthetic && !native {
                        self.annotated_methods.insert(triple);
                    }
                }
            }
        }
    }

    pub fn should_compile(&self, class: &SmaliClass, method: &SmaliMethod) -> bool {
        let cls = class.name.as_jni_type();
        let proto = method.signature.to_jni();
        let triple = (cls.clone(), method.name.clone(), proto);

        // Structural, and fatal when missed: DEX requires every method an
        // interface declares to be either abstract or to carry a body. A
        // converted method is `native` with no code -- neither -- so the
        // verifier rejects the whole dex file, not merely that method, and
        // every class in the APK stops loading with
        // "Interface method ... is not public and abstract".
        //
        // Kotlin makes this ordinary rather than exotic: a method with a
        // default argument gets its `$default` bridge declared *inside the
        // interface that declares the method*, so one `interface` with one
        // defaulted parameter is enough to lose an APK.
        if class.modifiers.contains(&Modifier::Interface) {
            return false;
        }
        if self.conflict_methods.contains(&triple) {
            return false;
        }
        if self.skip_synthetic && method.modifiers.contains(&Modifier::Synthetic) {
            return false;
        }
        if method.modifiers.contains(&Modifier::Native) {
            return false;
        }
        if method.name == "<clinit>" {
            return false;
        }
        if method.name == "<init>" && !self.allow_init {
            return false;
        }
        // A same-named native method makes ART's short-name lookup ambiguous.
        if self
            .native_methods
            .contains(&(cls.clone(), method.name.clone()))
        {
            return false;
        }

        let full_name = format!("{}{}{}", triple.0, triple.1, triple.2);
        for rule in &self.keep_filters {
            if rule.is_match(&full_name) {
                return false;
            }
        }
        if self.compile_full_match.contains(&full_name) {
            return true;
        }
        if self.annotated_methods.contains(&triple) {
            return true;
        }
        self.compile_filters.iter().any(|r| r.is_match(&full_name))
    }
}

/// `get_method_triple(m, return_type=False)`: keep only the parameter list, so
/// methods differing solely in return type land in the same bucket.
fn strip_return_type(proto: &str) -> String {
    match proto.find(')') {
        Some(index) => proto[..index + 1].to_string(),
        None => proto.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smali::types::{MethodSignature, ObjectIdentifier};

    /// A filter that compiles everything, with no scan pass run: the tests here
    /// are about the structural rules, not about what a filter file matches.
    fn catch_all() -> MethodFilter {
        MethodFilter {
            compile_filters: vec![Regex::new(".*").unwrap()],
            keep_filters: Vec::new(),
            compile_full_match: HashSet::new(),
            conflict_methods: HashSet::new(),
            native_methods: HashSet::new(),
            annotated_methods: HashSet::new(),
            skip_synthetic: false,
            allow_init: false,
        }
    }

    fn class_named(descriptor: &str, modifiers: Vec<Modifier>) -> SmaliClass {
        SmaliClass {
            name: ObjectIdentifier::from_jni_type(descriptor),
            modifiers,
            source: None,
            super_class: ObjectIdentifier::from_jni_type("Ljava/lang/Object;"),
            implements: Vec::new(),
            annotations: Vec::new(),
            fields: Vec::new(),
            methods: Vec::new(),
            file_path: None,
        }
    }

    fn method_named(name: &str, modifiers: Vec<Modifier>) -> SmaliMethod {
        SmaliMethod {
            name: name.to_string(),
            modifiers,
            constructor: false,
            signature: MethodSignature::from_jni("()V"),
            locals: 0,
            registers: None,
            params: Vec::new(),
            annotations: Vec::new(),
            ops: Vec::new(),
        }
    }

    #[test]
    fn interface_methods_are_never_converted() {
        let filter = catch_all();
        let iface = class_named(
            "Lcom/example/ApkSigning;",
            vec![Modifier::Public, Modifier::Interface],
        );

        // Both shapes Kotlin leaves in an interface: the declaration itself,
        // and the `$default` bridge a default argument generates beside it.
        assert!(!filter.should_compile(&iface, &method_named("sign", vec![Modifier::Public])));
        assert!(!filter.should_compile(
            &iface,
            &method_named("sign$default", vec![Modifier::Public, Modifier::Static])
        ));
    }

    #[test]
    fn the_same_method_in_a_class_is_converted() {
        let filter = catch_all();
        let class = class_named("Lcom/example/ApkSigning;", vec![Modifier::Public]);
        assert!(filter.should_compile(&class, &method_named("sign", vec![Modifier::Public])));
    }

    #[test]
    fn strip_return_type_keeps_params() {
        assert_eq!(strip_return_type("(II)I"), "(II)");
        assert_eq!(strip_return_type("()V"), "()");
    }
}
