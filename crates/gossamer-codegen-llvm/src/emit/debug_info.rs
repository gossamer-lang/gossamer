//! DWARF debug metadata for `gos build -g`.

use std::collections::HashMap;
use std::fmt::Write;

use gossamer_mir::Body;

use super::{
    DEBUG_LOCATION_MARKER, DEBUG_VARIABLE_MARKER, OptProfile, first_llvm_symbol, opt_profile,
    source_position, want_reproducible,
};

/// Debug-info metadata nodes numbered as they are first needed.
struct DebugMetadata {
    directory: String,
    lines: Vec<String>,
    files: HashMap<String, u32>,
    locations: HashMap<(u32, u32, u32), u32>,
    types: HashMap<String, u32>,
    next_id: u32,
}

impl DebugMetadata {
    fn fresh(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// The type node for a [`DEBUG_VARIABLE_MARKER`] type token.
    fn debug_type(&mut self, token: &str) -> u32 {
        if let Some(&id) = self.types.get(token) {
            return id;
        }
        let basic = |name: &str, bits: u32, encoding: &str| {
            format!("!DIBasicType(name: \"{name}\", size: {bits}, encoding: {encoding})")
        };
        let node = match token {
            "bool" => basic("bool", 8, "DW_ATE_boolean"),
            "char" => basic("char32_t", 32, "DW_ATE_UTF"),
            "f32" => basic("f32", 32, "DW_ATE_float"),
            "f64" => basic("f64", 64, "DW_ATE_float"),
            _ if token.starts_with('s') || token.starts_with('u') => {
                let signed = token.starts_with('s');
                let bits: u32 = token[1..].parse().unwrap_or(64);
                let name = format!("{}{bits}", if signed { "i" } else { "u" });
                basic(
                    &name,
                    bits,
                    if signed {
                        "DW_ATE_signed"
                    } else {
                        "DW_ATE_unsigned"
                    },
                )
            }
            _ => {
                if let Some((words, name)) = token
                    .strip_prefix("words")
                    .and_then(|rest| rest.split_once(':'))
                {
                    let count: u32 = words.parse().unwrap_or(1);
                    let word = self.debug_type("u64");
                    let range = self.fresh();
                    self.lines
                        .push(format!("!{range} = !{{!DISubrange(count: {count})}}"));
                    format!(
                        "!DICompositeType(tag: DW_TAG_array_type, name: \"{}\", baseType: !{word}, \
                         size: {bits}, elements: !{range})",
                        escape_metadata_string(name),
                        bits = count * 64,
                    )
                } else {
                    let name = token.strip_prefix("ptr:").unwrap_or(token);
                    format!(
                        "!DIDerivedType(tag: DW_TAG_pointer_type, name: \"{}\", baseType: null, size: 64)",
                        escape_metadata_string(name)
                    )
                }
            }
        };
        let id = self.fresh();
        self.lines.push(format!("!{id} = {node}"));
        self.types.insert(token.to_string(), id);
        id
    }

    fn file(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.files.get(name) {
            return id;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.lines.push(format!(
            "!{id} = !DIFile(filename: \"{}\", directory: \"{}\")",
            escape_metadata_string(name),
            escape_metadata_string(&self.directory),
        ));
        self.files.insert(name.to_string(), id);
        id
    }

    fn location(&mut self, line: u32, column: u32, scope: u32) -> u32 {
        if let Some(&id) = self.locations.get(&(line, column, scope)) {
            return id;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.lines.push(format!(
            "!{id} = !DILocation(line: {line}, column: {column}, scope: !{scope})"
        ));
        self.locations.insert((line, column, scope), id);
        id
    }
}

/// `text` as the body of an LLVM metadata string, with quotes and
/// backslashes escaped.
fn escape_metadata_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '"' | '\\' => {
                let _ = write!(out, "\\{:02X}", u32::from(ch));
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Byte offset of the `;` that starts a trailing comment on an IR line, or
/// the line's length when it has none. A `;` inside a quoted symbol or
/// string is not a comment.
fn ir_comment_start(line: &str) -> usize {
    let mut in_string = false;
    for (index, byte) in line.bytes().enumerate() {
        match byte {
            b'"' => in_string = !in_string,
            b';' if !in_string => return index,
            _ => {}
        }
    }
    line.len()
}

/// Emits DWARF debug-info metadata for every body in `bodies` and gives each
/// of their instructions a source location.
///
/// Each body gets a `DISubprogram` in the file and at the line it was written
/// in, attached to its `define`; every instruction in it carries a
/// `DILocation` taken from the nearest [`DEBUG_LOCATION_MARKER`] above it, or
/// the function's own position before the first. LLVM drops a module's debug
/// info when an inlinable call in a function that has some lacks a location,
/// so a function either has locations on all of its instructions or no
/// subprogram at all.
pub(super) fn emit_dwarf_metadata(out: &mut String, bodies: &[Body]) {
    let directory = if want_reproducible() {
        ".".to_string()
    } else {
        std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(str::to_string))
            .unwrap_or_else(|| ".".to_string())
    };
    let first_free = 100u32.saturating_add(u32::try_from(bodies.len()).unwrap_or(u32::MAX));
    let mut meta = DebugMetadata {
        directory,
        lines: Vec::new(),
        files: HashMap::new(),
        locations: HashMap::new(),
        types: HashMap::new(),
        next_id: first_free,
    };
    let mut declares_variables = false;
    let mut subprograms: HashMap<String, (u32, u32, u32)> = HashMap::new();
    let mut subprogram_files: HashMap<u32, u32> = HashMap::new();
    let mut unit_file = None;
    for (idx, body) in bodies.iter().enumerate() {
        let id = 100u32 + u32::try_from(idx).unwrap_or(u32::MAX);
        let (file, line, column) =
            source_position(body.span.start).unwrap_or_else(|| ("main.gos".to_string(), 1, 1));
        let file_id = meta.file(&file);
        if body.name == "main" || unit_file.is_none() {
            unit_file = Some(file_id);
        }
        let llvm_name = crate::lower::mangle_fn_name(&body.name).into_owned();
        meta.lines.push(format!(
            "!{id} = distinct !DISubprogram(name: \"{name}\", linkageName: \"{lname}\", \
             scope: !{file_id}, file: !{file_id}, line: {line}, type: !52, scopeLine: {line}, \
             spFlags: DISPFlagDefinition, unit: !51)",
            name = escape_metadata_string(&body.name),
            lname = escape_metadata_string(&llvm_name),
        ));
        subprograms.insert(llvm_name, (id, line, column));
        subprogram_files.insert(id, file_id);
    }
    let unit_file = unit_file.unwrap_or_else(|| meta.file("main.gos"));

    let mut rewritten = String::with_capacity(out.len() + out.len() / 3);
    // The subprogram of the function being walked and the position its next
    // instruction is at.
    let mut current: Option<(u32, u32, u32)> = None;
    let mut in_switch = false;
    for line in out.lines() {
        if line.starts_with("define ") {
            current = first_llvm_symbol(line)
                .and_then(|symbol| subprograms.get(&symbol).copied())
                .filter(|_| line.ends_with(" {"));
            if let (Some((scope, _, _)), Some(head)) = (current, line.strip_suffix(" {")) {
                let _ = writeln!(rewritten, "{head} !dbg !{scope} {{");
            } else {
                rewritten.push_str(line);
                rewritten.push('\n');
            }
            continue;
        }
        let Some((scope, at_line, at_column)) = current.as_mut() else {
            rewritten.push_str(line);
            rewritten.push('\n');
            continue;
        };
        let trimmed = line.trim_start();
        if line == "}" {
            current = None;
            in_switch = false;
        } else if let Some(variable) = trimmed.strip_prefix(DEBUG_VARIABLE_MARKER) {
            let parts: Vec<&str> = variable.split_whitespace().collect();
            if let [slot, arg, var_line, var_column, ty, name] = parts.as_slice()
                && let (Ok(arg), Ok(var_line), Ok(var_column)) = (
                    arg.parse::<u32>(),
                    var_line.parse::<u32>(),
                    var_column.parse::<u32>(),
                )
            {
                let scope_id = *scope;
                let file = subprogram_files
                    .get(&scope_id)
                    .copied()
                    .unwrap_or(unit_file);
                let type_id = meta.debug_type(ty);
                let var_id = meta.fresh();
                let arg_field = if arg > 0 {
                    format!("arg: {arg}, ")
                } else {
                    String::new()
                };
                meta.lines.push(format!(
                    "!{var_id} = !DILocalVariable(name: \"{}\", {arg_field}scope: !{scope_id}, \
                     file: !{file}, line: {var_line}, type: !{type_id})",
                    escape_metadata_string(name)
                ));
                let at = meta.location(var_line, var_column, scope_id);
                let _ = writeln!(
                    rewritten,
                    "  call void @llvm.dbg.declare(metadata ptr {slot}, metadata !{var_id}, \
                     metadata !DIExpression()), !dbg !{at}"
                );
                declares_variables = true;
            }
            continue;
        } else if let Some(position) = trimmed.strip_prefix(DEBUG_LOCATION_MARKER) {
            let mut parts = position.split_whitespace().map(str::parse::<u32>);
            if let (Some(Ok(l)), Some(Ok(c))) = (parts.next(), parts.next()) {
                *at_line = l;
                *at_column = c;
            }
            continue;
        } else if in_switch {
            // A `switch`'s case list spans lines; its location follows the
            // closing bracket.
            if trimmed == "]" {
                in_switch = false;
                let id = meta.location(*at_line, *at_column, *scope);
                let _ = writeln!(rewritten, "{line}, !dbg !{id}");
                continue;
            }
        } else if line.starts_with("  ") && !trimmed.is_empty() && !trimmed.starts_with(';') {
            let end = ir_comment_start(line);
            let code = line[..end].trim_end();
            if code.ends_with('[') {
                in_switch = true;
            } else {
                let id = meta.location(*at_line, *at_column, *scope);
                let _ = writeln!(rewritten, "{code}, !dbg !{id}{}", &line[end..]);
                continue;
            }
        }
        rewritten.push_str(line);
        rewritten.push('\n');
    }
    *out = rewritten;

    writeln!(out).unwrap();
    if declares_variables && !out.contains("declare void @llvm.dbg.declare(") {
        writeln!(
            out,
            "declare void @llvm.dbg.declare(metadata, metadata, metadata)"
        )
        .unwrap();
    }
    writeln!(out, "!llvm.module.flags = !{{!40, !41}}").unwrap();
    writeln!(out, "!llvm.dbg.cu = !{{!51}}").unwrap();
    writeln!(out, "!40 = !{{i32 7, !\"Dwarf Version\", i32 4}}").unwrap();
    writeln!(out, "!41 = !{{i32 2, !\"Debug Info Version\", i32 3}}").unwrap();
    writeln!(
        out,
        "!51 = distinct !DICompileUnit(language: DW_LANG_C99, file: !{unit_file}, \
         producer: \"gossamer {version}\", isOptimized: {optimized}, runtimeVersion: 0, \
         emissionKind: FullDebug)",
        version = env!("CARGO_PKG_VERSION"),
        optimized = matches!(opt_profile(), OptProfile::Release),
    )
    .unwrap();
    writeln!(out, "!52 = !DISubroutineType(types: !{{}})").unwrap();
    for line in meta.lines {
        writeln!(out, "{line}").unwrap();
    }
}
