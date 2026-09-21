//! Report what a filter file selects from an APK, and optionally the size of the
//! code generated for each method:
//!
//! ```text
//! cargo run -p dex2rs --example dex_stats -- app-release.apk filter.txt
//! cargo run -p dex2rs --example dex_stats -- app-release.apk filter.txt --sizes > ours.tsv
//! python3 tools/golden_cpp.py --apk app-release.apk --filter filter.txt > theirs.tsv
//! ```
//!
//! The `--sizes` output uses the Python tool's `OK<TAB>full_name<TAB>bytes`
//! format, so the two runs can be joined and compared per method.

use std::path::Path;

use smali::types::SmaliOp;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1).filter(|a| a != "--sizes");
    let sizes = std::env::args().any(|a| a == "--sizes");
    let input_path = args.next().unwrap_or_else(|| "app-release.apk".to_string());
    let filter_path = args.next().unwrap_or_else(|| "filter.txt".to_string());

    let input = dex2rs::apk::load(Path::new(&input_path))?;
    let classes: Vec<smali::types::SmaliClass> = input
        .dexes()
        .iter()
        .flat_map(|unit| unit.classes.iter().cloned())
        .collect();
    let filter =
        dex2rs::filter::MethodFilter::new(Path::new(&filter_path), &classes, false, false)?;

    let mut total_methods = 0;
    let mut total_with_code = 0;
    let mut matches = 0;
    let mut matches_with_code = 0;
    let mut generated = 0usize;
    let mut generated_bytes = 0usize;
    // How many of the methods actually compiled carry a JNI long name past 220
    // characters -- i.e. how many the old converter silently dropped. Reported
    // for information only; nothing is skipped on this basis any more.
    let mut long_names = 0usize;

    for class in &classes {
        total_methods += class.methods.len();
        for method in &class.methods {
            let has_code = method.ops.iter().any(|op| matches!(op, SmaliOp::Op(_)));
            if has_code {
                total_with_code += 1;
            }
            if !filter.should_compile(class, method) {
                continue;
            }
            matches += 1;
            if !has_code {
                continue;
            }
            matches_with_code += 1;
            // No JNI-name length limit: the old converter lost every method
            // whose mangled name passed 220 characters, so counting them as
            // "skipped" would misreport those methods as unconvertible.
            if dex2rs_core::util::jni_long_name(
                &class.name.as_jni_type(),
                &method.name,
                &method.signature.to_jni(),
            )
            .len()
                > 220
            {
                long_names += 1;
            }
            let source = compile(class, method)?;
            generated += 1;
            generated_bytes += source.len();
            if sizes {
                println!(
                    "OK\t{}{}{}\t{}",
                    class.name.as_jni_type(),
                    method.name,
                    method.signature.to_jni(),
                    source.len()
                );
            }
        }
    }

    println!("# classes: {}", classes.len());
    println!("# methods: {total_methods} ({total_with_code} with code)");
    println!("# filter selects: {matches} ({matches_with_code} with code)");
    println!("# compiled: {generated}");
    println!(
        "#   of which {long_names} have a JNI long name > 220 chars \
         (the old converter silently dropped these)"
    );
    println!(
        "# generated: {generated_bytes} bytes ({:.0}/method)",
        generated_bytes as f64 / generated.max(1) as f64
    );
    Ok(())
}

fn compile(
    class: &smali::types::SmaliClass,
    method: &smali::types::SmaliMethod,
) -> Result<String, String> {
    let input = dex2rs_core::dexinput::decode_method(class, method, false, false)?;
    let irmethod = dex2rs_core::ssa::IrBuilder::new(&input).process()?;
    irmethod.get_source()
}
