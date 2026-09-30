//! Audits rendered IR for symbols the module references but never declares or defines.

use anyhow::{Context, Result, anyhow};

pub(super) fn audit_llvm_ir_symbols(ll_path: &std::path::Path) -> Result<()> {
    let ir = std::fs::read_to_string(ll_path)
        .with_context(|| format!("reading {}", ll_path.display()))?;
    audit_llvm_ir_symbols_text(&ir)
        .with_context(|| format!("auditing LLVM IR symbols in {}", ll_path.display()))
}

fn audit_llvm_ir_symbols_text(ir: &str) -> Result<()> {
    let mut defined = std::collections::HashSet::new();
    for line in ir.lines() {
        let trimmed = line.trim_start();
        if (trimmed.starts_with("define ") || trimmed.starts_with("declare "))
            && let Some(symbol) = first_llvm_symbol(trimmed)
        {
            defined.insert(symbol);
        } else if trimmed.starts_with('@')
            && trimmed.contains(" = ")
            && let Some(symbol) = first_llvm_symbol(trimmed)
        {
            defined.insert(symbol);
        }
    }

    let mut missing: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        std::collections::BTreeMap::new();
    let mut current_fn: Option<String> = None;
    for line in ir.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("define ") {
            current_fn = first_llvm_symbol(trimmed);
        } else if trimmed == "}" {
            current_fn = None;
        }
        for symbol in llvm_symbols_in_line(line) {
            if symbol.starts_with("llvm.") || defined.contains(&symbol) {
                continue;
            }
            let scope = current_fn.clone().unwrap_or_else(|| "<module>".to_string());
            missing.entry(symbol).or_default().insert(scope);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    let summary = missing
        .into_iter()
        .map(|(symbol, scopes)| {
            let scopes = scopes.into_iter().collect::<Vec<_>>().join(", ");
            format!("@{symbol} referenced from {scopes}")
        })
        .collect::<Vec<_>>()
        .join("; ");
    Err(anyhow!(
        "llvm backend internal lowering bug: undefined symbols before LLVM tools: {summary}"
    ))
}

pub(super) fn first_llvm_symbol(line: &str) -> Option<String> {
    llvm_symbols_in_line(line).into_iter().next()
}

fn llvm_symbols_in_line(line: &str) -> Vec<String> {
    let bytes = line.as_bytes();
    let mut symbols = Vec::new();
    let mut i = 0usize;
    let mut in_string = false;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' && (i == 0 || bytes[i - 1] != b'\\') {
            in_string = !in_string;
            i += 1;
            continue;
        }
        if !in_string && b == b';' {
            break;
        }
        if !in_string && b == b'@' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                let mut j = i + 2;
                let mut out = String::new();
                while j < bytes.len() {
                    if bytes[j] == b'"' && bytes[j - 1] != b'\\' {
                        break;
                    }
                    out.push(bytes[j] as char);
                    j += 1;
                }
                if !out.is_empty() {
                    symbols.push(out);
                }
                i = j.saturating_add(1);
                continue;
            }
            let mut j = i + 1;
            while j < bytes.len() && is_llvm_symbol_byte(bytes[j]) {
                j += 1;
            }
            if j > i + 1 {
                symbols.push(line[i + 1..j].to_string());
            }
            i = j;
            continue;
        }
        i += 1;
    }
    symbols
}

fn is_llvm_symbol_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'$' | b'-')
}

#[cfg(test)]
mod symbol_audit_tests {
    use super::audit_llvm_ir_symbols_text;

    #[test]
    fn audit_accepts_defined_declared_and_global_symbols() {
        let ir = r#"
; ModuleID = "gossamer"
@G = private constant [3 x i8] c"a@b"
declare void @puts(ptr)
define void @"main"() {
entry:
  call void @puts(ptr @G)
  ret void
}
"#;
        audit_llvm_ir_symbols_text(ir).expect("IR symbols should be complete");
    }

    #[test]
    fn audit_reports_missing_symbol_with_function_context() {
        let ir = r#"
define void @"main"() {
entry:
  %t0 = call i1 @"is_halted"(ptr null)
  ret void
}
"#;
        let err = audit_llvm_ir_symbols_text(ir).unwrap_err().to_string();
        assert!(err.contains("@is_halted referenced from main"), "{err}");
        assert!(err.contains("undefined symbols before LLVM tools"), "{err}");
    }

    #[test]
    fn audit_ignores_symbols_inside_string_constants_and_comments() {
        let ir = r#"
@S = private constant [17 x i8] c"user@example.com\00"
define void @"main"() {
entry:
  ; @missing_in_comment
  ret void
}
"#;
        audit_llvm_ir_symbols_text(ir).expect("string/comment @ signs are not symbol refs");
    }
}
