#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeSymbol {
    pub name: String,
    pub qualified_name: String,
    pub kind: String,
    pub signature: String,
    pub start_line: usize,
    pub end_line: usize,
    pub depth: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AstLanguage {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Go,
}

pub fn detect_language(ext: &str) -> Option<AstLanguage> {
    match ext.trim_start_matches('.').to_ascii_lowercase().as_str() {
        "rs" => Some(AstLanguage::Rust),
        "ts" | "mts" | "cts" => Some(AstLanguage::TypeScript),
        "tsx" => Some(AstLanguage::Tsx),
        "js" | "jsx" | "mjs" | "cjs" => Some(AstLanguage::JavaScript),
        "py" | "pyi" => Some(AstLanguage::Python),
        "go" => Some(AstLanguage::Go),
        _ => None,
    }
}

pub fn format_outline_text(rel_path: &str, symbols: &[CodeSymbol], max_symbols: usize) -> (String, usize) {
    let limit = max_symbols.clamp(1, 500);
    let shown = symbols.len().min(limit);
    let mut lines = Vec::with_capacity(shown + 2);
    lines.push(format!("AST code outline: {} ({} symbols)", rel_path, symbols.len()));
    for sym in symbols.iter().take(limit) {
        let indent = "  ".repeat(sym.depth.min(4));
        lines.push(format!(
            "{indent}L{}-L{} [{}] {} — {}",
            sym.start_line, sym.end_line, sym.kind, sym.qualified_name, sym.signature
        ));
    }
    if symbols.len() > limit {
        lines.push(format!(
            "... truncated ({} more symbols, total {}).",
            symbols.len() - limit,
            symbols.len()
        ));
    }
    (lines.join("\n"), symbols.len())
}

pub fn read_symbol_from_content(
    rel_path: &str,
    ext: &str,
    content: &str,
    query: &str,
    include_docs: bool,
    max_lines: usize,
) -> Result<String, String> {
    let symbols = extract_ast_symbols(ext, content).ok_or_else(|| {
        format!(
            "AST semantic parser does not support extension '.{}' (supported: rs, ts, tsx, js, jsx, py, go).",
            ext.trim_start_matches('.')
        )
    })?;

    if symbols.is_empty() {
        return Err(format!("No AST symbols found in '{}'.", rel_path));
    }

    let trimmed_query = query.trim();
    if trimmed_query.is_empty() {
        return Err("Parameter 'symbol' cannot be empty.".to_string());
    }
    let normalized_query = normalize_symbol_query(trimmed_query);

    let mut matches: Vec<&CodeSymbol> = symbols
        .iter()
        .filter(|s| normalize_symbol_query(&s.qualified_name) == normalized_query)
        .collect();

    if matches.is_empty() {
        matches = symbols.iter().filter(|s| s.name == trimmed_query).collect();
    }
    if matches.is_empty() {
        let lower_q = normalized_query.to_ascii_lowercase();
        matches = symbols
            .iter()
            .filter(|s| {
                normalize_symbol_query(&s.qualified_name).to_ascii_lowercase() == lower_q
                    || s.name.to_ascii_lowercase() == lower_q
            })
            .collect();
    }

    if matches.is_empty() {
        let available: Vec<String> = symbols
            .iter()
            .take(25)
            .map(|s| format!("{} (L{})", s.qualified_name, s.start_line))
            .collect();
        let suffix = if symbols.len() > 25 {
            format!(" ... (+{} more)", symbols.len() - 25)
        } else {
            String::new()
        };
        return Err(format!(
            "Symbol '{}' not found in '{}'. Available symbols: {}{}",
            trimmed_query,
            rel_path,
            available.join(", "),
            suffix
        ));
    }

    let chosen = matches[0];
    let file_lines: Vec<&str> = content.lines().collect();
    if file_lines.is_empty() {
        return Err(format!("File '{}' is empty.", rel_path));
    }

    let sym_start_idx = chosen.start_line.saturating_sub(1).min(file_lines.len().saturating_sub(1));
    let sym_end_idx = chosen.end_line.saturating_sub(1).min(file_lines.len().saturating_sub(1));
    let display_start_idx = if include_docs {
        find_doc_comment_start(&file_lines, sym_start_idx)
    } else {
        sym_start_idx
    };

    let line_cap = max_lines.clamp(5, 500);
    let total_span = sym_end_idx.saturating_sub(display_start_idx) + 1;
    let slice_end_idx = if total_span > line_cap {
        (display_start_idx + line_cap - 1).min(sym_end_idx)
    } else {
        sym_end_idx
    };

    let mut out = String::new();
    out.push_str(&format!(
        "Symbol: {} [{}] in {} (L{}-L{})\n",
        chosen.qualified_name,
        chosen.kind,
        rel_path,
        display_start_idx + 1,
        sym_end_idx + 1
    ));

    if matches.len() > 1 {
        let others: Vec<String> = matches
            .iter()
            .skip(1)
            .take(5)
            .map(|m| format!("{} (L{}-L{})", m.qualified_name, m.start_line, m.end_line))
            .collect();
        out.push_str(&format!(
            "Note: {} matches for '{}'; showing first match. Other matches: {}\n",
            matches.len(),
            trimmed_query,
            others.join(", ")
        ));
    }

    out.push_str("---\n");
    for idx in display_start_idx..=slice_end_idx {
        out.push_str(&format!("{:>4} | {}\n", idx + 1, file_lines[idx]));
    }
    if slice_end_idx < sym_end_idx {
        out.push_str(&format!(
            "... truncated after {} lines ({} more lines up to L{}; raise max_lines up to 500 or use read_file).\n",
            line_cap,
            sym_end_idx - slice_end_idx,
            sym_end_idx + 1
        ));
    }

    Ok(out)
}

fn normalize_symbol_query(raw: &str) -> String {
    raw.trim().replace('.', "::")
}

fn find_doc_comment_start(lines: &[&str], sym_start_idx: usize) -> usize {
    let mut cursor = sym_start_idx;
    let mut in_block_comment = false;

    while cursor > 0 {
        let prev_idx = cursor - 1;
        let trimmed = lines[prev_idx].trim();
        if trimmed.is_empty() {
            break;
        }
        if in_block_comment {
            cursor = prev_idx;
            if trimmed.starts_with("/*") {
                in_block_comment = false;
            }
            continue;
        }
        if trimmed.ends_with("*/") && !trimmed.starts_with("/*") {
            in_block_comment = true;
            cursor = prev_idx;
            continue;
        }
        if trimmed.starts_with("///")
            || trimmed.starts_with("//!")
            || trimmed.starts_with("//")
            || trimmed.starts_with("/**")
            || trimmed.starts_with("/*")
            || trimmed.starts_with("#[")
            || trimmed.starts_with('#')
            || trimmed.starts_with('@')
        {
            cursor = prev_idx;
            continue;
        }
        break;
    }

    cursor
}

#[cfg(feature = "semantic-ast")]
pub fn extract_ast_symbols(ext: &str, content: &str) -> Option<Vec<CodeSymbol>> {
    let lang = detect_language(ext)?;
    let mut parser = tree_sitter::Parser::new();
    let ts_lang: tree_sitter::Language = match lang {
        AstLanguage::Rust => tree_sitter_rust::LANGUAGE.into(),
        AstLanguage::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        AstLanguage::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        AstLanguage::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        AstLanguage::Python => tree_sitter_python::LANGUAGE.into(),
        AstLanguage::Go => tree_sitter_go::LANGUAGE.into(),
    };
    parser.set_language(&ts_lang).ok()?;
    let tree = parser.parse(content, None)?;
    let bytes = content.as_bytes();
    let mut symbols = Vec::new();
    let mut scope: Vec<String> = Vec::new();

    match lang {
        AstLanguage::Rust => collect_rust_nodes(tree.root_node(), bytes, &mut scope, &mut symbols),
        AstLanguage::TypeScript | AstLanguage::Tsx | AstLanguage::JavaScript => {
            collect_ts_js_nodes(tree.root_node(), bytes, &mut scope, &mut symbols)
        }
        AstLanguage::Python => collect_python_nodes(tree.root_node(), bytes, &mut scope, &mut symbols),
        AstLanguage::Go => collect_go_nodes(tree.root_node(), bytes, &mut symbols),
    }

    Some(symbols)
}

#[cfg(not(feature = "semantic-ast"))]
pub fn extract_ast_symbols(_ext: &str, _content: &str) -> Option<Vec<CodeSymbol>> {
    None
}

#[cfg(feature = "semantic-ast")]
fn node_text<'a>(node: tree_sitter::Node<'a>, bytes: &'a [u8]) -> &'a str {
    node.utf8_text(bytes).unwrap_or("")
}

#[cfg(feature = "semantic-ast")]
fn first_signature_line(node: tree_sitter::Node<'_>, bytes: &[u8]) -> String {
    let raw = node_text(node, bytes);
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("#[") || trimmed.starts_with('@') {
            continue;
        }
        let cleaned = trimmed.trim_end_matches('{').trim_end().to_string();
        if cleaned.chars().count() > 120 {
            let truncated: String = cleaned.chars().take(117).collect();
            return format!("{}...", truncated);
        }
        return cleaned;
    }
    String::new()
}

#[cfg(feature = "semantic-ast")]
fn push_symbol(
    symbols: &mut Vec<CodeSymbol>,
    scope: &[String],
    name: String,
    kind: &str,
    node: tree_sitter::Node<'_>,
    bytes: &[u8],
) {
    let qualified_name = if scope.is_empty() {
        name.clone()
    } else {
        format!("{}::{}", scope.join("::"), name)
    };
    symbols.push(CodeSymbol {
        name,
        qualified_name,
        kind: kind.to_string(),
        signature: first_signature_line(node, bytes),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        depth: scope.len(),
    });
}

#[cfg(feature = "semantic-ast")]
fn collect_rust_nodes(
    node: tree_sitter::Node<'_>,
    bytes: &[u8],
    scope: &mut Vec<String>,
    symbols: &mut Vec<CodeSymbol>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_item" | "function_signature_item" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = node_text(name_node, bytes).trim().to_string();
                    if !name.is_empty() {
                        push_symbol(symbols, scope, name, "fn", child, bytes);
                    }
                }
            }
            "struct_item" | "enum_item" | "type_item" | "const_item" | "static_item" => {
                let kind = match child.kind() {
                    "struct_item" => "struct",
                    "enum_item" => "enum",
                    "type_item" => "type",
                    "const_item" => "const",
                    "static_item" => "static",
                    _ => "item",
                };
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = node_text(name_node, bytes).trim().to_string();
                    if !name.is_empty() {
                        push_symbol(symbols, scope, name, kind, child, bytes);
                    }
                }
            }
            "macro_definition" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = node_text(name_node, bytes).trim().to_string();
                    if !name.is_empty() {
                        push_symbol(symbols, scope, name, "macro", child, bytes);
                    }
                }
            }
            "trait_item" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = node_text(name_node, bytes).trim().to_string();
                    if !name.is_empty() {
                        push_symbol(symbols, scope, name.clone(), "trait", child, bytes);
                        if let Some(body) = child.child_by_field_name("body") {
                            scope.push(name);
                            collect_rust_nodes(body, bytes, scope, symbols);
                            scope.pop();
                        }
                    }
                }
            }
            "impl_item" => {
                let type_name = child
                    .child_by_field_name("type")
                    .map(|n| clean_rust_type_name(node_text(n, bytes)))
                    .unwrap_or_default();
                let trait_name = child
                    .child_by_field_name("trait")
                    .map(|n| clean_rust_type_name(node_text(n, bytes)));
                let display_name = match (&trait_name, type_name.is_empty()) {
                    (Some(tr), false) => format!("{} for {}", tr, type_name),
                    (None, false) => type_name.clone(),
                    _ => String::new(),
                };
                if !display_name.is_empty() {
                    push_symbol(symbols, scope, display_name, "impl", child, bytes);
                }
                if let Some(body) = child.child_by_field_name("body") {
                    let pushed = if !type_name.is_empty() {
                        scope.push(type_name);
                        true
                    } else {
                        false
                    };
                    collect_rust_nodes(body, bytes, scope, symbols);
                    if pushed {
                        scope.pop();
                    }
                }
            }
            "mod_item" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = node_text(name_node, bytes).trim().to_string();
                    if !name.is_empty() {
                        push_symbol(symbols, scope, name.clone(), "mod", child, bytes);
                        if let Some(body) = child.child_by_field_name("body") {
                            scope.push(name);
                            collect_rust_nodes(body, bytes, scope, symbols);
                            scope.pop();
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(feature = "semantic-ast")]
fn clean_rust_type_name(raw: &str) -> String {
    raw.split('<')
        .next()
        .unwrap_or(raw)
        .trim()
        .rsplit("::")
        .next()
        .unwrap_or(raw)
        .trim()
        .to_string()
}

#[cfg(feature = "semantic-ast")]
fn collect_ts_js_nodes(
    node: tree_sitter::Node<'_>,
    bytes: &[u8],
    scope: &mut Vec<String>,
    symbols: &mut Vec<CodeSymbol>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        handle_ts_js_statement(child, child, bytes, scope, symbols);
    }
}

#[cfg(feature = "semantic-ast")]
fn handle_ts_js_statement(
    stmt: tree_sitter::Node<'_>,
    outer: tree_sitter::Node<'_>,
    bytes: &[u8],
    scope: &mut Vec<String>,
    symbols: &mut Vec<CodeSymbol>,
) {
    match stmt.kind() {
        "export_statement" => {
            if let Some(decl) = stmt.child_by_field_name("declaration") {
                handle_ts_js_statement(decl, stmt, bytes, scope, symbols);
            } else {
                let mut cursor = stmt.walk();
                for inner in stmt.children(&mut cursor) {
                    if matches!(
                        inner.kind(),
                        "function_declaration"
                            | "generator_function_declaration"
                            | "class_declaration"
                            | "abstract_class_declaration"
                            | "interface_declaration"
                            | "type_alias_declaration"
                            | "enum_declaration"
                            | "lexical_declaration"
                            | "variable_declaration"
                    ) {
                        handle_ts_js_statement(inner, stmt, bytes, scope, symbols);
                    }
                }
            }
        }
        "function_declaration" | "generator_function_declaration" => {
            if let Some(name_node) = stmt.child_by_field_name("name") {
                let name = node_text(name_node, bytes).trim().to_string();
                if !name.is_empty() {
                    push_symbol(symbols, scope, name, "function", outer, bytes);
                }
            }
        }
        "interface_declaration" | "type_alias_declaration" | "enum_declaration" => {
            let kind = match stmt.kind() {
                "interface_declaration" => "interface",
                "type_alias_declaration" => "type",
                "enum_declaration" => "enum",
                _ => "type",
            };
            if let Some(name_node) = stmt.child_by_field_name("name") {
                let name = node_text(name_node, bytes).trim().to_string();
                if !name.is_empty() {
                    push_symbol(symbols, scope, name, kind, outer, bytes);
                }
            }
        }
        "class_declaration" | "abstract_class_declaration" => {
            if let Some(name_node) = stmt.child_by_field_name("name") {
                let name = node_text(name_node, bytes).trim().to_string();
                if !name.is_empty() {
                    push_symbol(symbols, scope, name.clone(), "class", outer, bytes);
                    if let Some(body) = stmt.child_by_field_name("body") {
                        scope.push(name);
                        collect_ts_js_class_body(body, bytes, scope, symbols);
                        scope.pop();
                    }
                }
            }
        }
        "lexical_declaration" | "variable_declaration" => {
            let mut cursor = stmt.walk();
            for child in stmt.children(&mut cursor) {
                if child.kind() == "variable_declarator" {
                    let is_callable = child
                        .child_by_field_name("value")
                        .map(|v| matches!(v.kind(), "arrow_function" | "function_expression" | "function"))
                        .unwrap_or(false);
                    if is_callable {
                        if let Some(name_node) = child.child_by_field_name("name") {
                            let name = node_text(name_node, bytes).trim().to_string();
                            if !name.is_empty() {
                                push_symbol(symbols, scope, name, "function", outer, bytes);
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(feature = "semantic-ast")]
fn collect_ts_js_class_body(
    body: tree_sitter::Node<'_>,
    bytes: &[u8],
    scope: &mut Vec<String>,
    symbols: &mut Vec<CodeSymbol>,
) {
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if matches!(child.kind(), "method_definition" | "method_signature" | "abstract_method_signature") {
            if let Some(name_node) = child.child_by_field_name("name") {
                let name = node_text(name_node, bytes).trim().to_string();
                if !name.is_empty() {
                    push_symbol(symbols, scope, name, "method", child, bytes);
                }
            }
        } else if matches!(child.kind(), "public_field_definition" | "field_definition") {
            let is_callable = child
                .child_by_field_name("value")
                .map(|v| matches!(v.kind(), "arrow_function" | "function_expression"))
                .unwrap_or(false);
            if is_callable {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = node_text(name_node, bytes).trim().to_string();
                    if !name.is_empty() {
                        push_symbol(symbols, scope, name, "method", child, bytes);
                    }
                }
            }
        }
    }
}

#[cfg(feature = "semantic-ast")]
fn collect_python_nodes(
    node: tree_sitter::Node<'_>,
    bytes: &[u8],
    scope: &mut Vec<String>,
    symbols: &mut Vec<CodeSymbol>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        handle_python_item(child, child, bytes, scope, symbols);
    }
}

#[cfg(feature = "semantic-ast")]
fn handle_python_item(
    item: tree_sitter::Node<'_>,
    outer: tree_sitter::Node<'_>,
    bytes: &[u8],
    scope: &mut Vec<String>,
    symbols: &mut Vec<CodeSymbol>,
) {
    match item.kind() {
        "decorated_definition" => {
            if let Some(def) = item.child_by_field_name("definition") {
                handle_python_item(def, item, bytes, scope, symbols);
            }
        }
        "function_definition" => {
            if let Some(name_node) = item.child_by_field_name("name") {
                let name = node_text(name_node, bytes).trim().to_string();
                if !name.is_empty() {
                    let kind = if scope.is_empty() { "def" } else { "method" };
                    push_symbol(symbols, scope, name, kind, outer, bytes);
                }
            }
        }
        "class_definition" => {
            if let Some(name_node) = item.child_by_field_name("name") {
                let name = node_text(name_node, bytes).trim().to_string();
                if !name.is_empty() {
                    push_symbol(symbols, scope, name.clone(), "class", outer, bytes);
                    if let Some(body) = item.child_by_field_name("body") {
                        scope.push(name);
                        collect_python_nodes(body, bytes, scope, symbols);
                        scope.pop();
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(feature = "semantic-ast")]
fn collect_go_nodes(node: tree_sitter::Node<'_>, bytes: &[u8], symbols: &mut Vec<CodeSymbol>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_declaration" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = node_text(name_node, bytes).trim().to_string();
                    if !name.is_empty() {
                        push_symbol(symbols, &[], name, "func", child, bytes);
                    }
                }
            }
            "method_declaration" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = node_text(name_node, bytes).trim().to_string();
                    let receiver = child
                        .child_by_field_name("receiver")
                        .map(|r| extract_go_receiver_type(node_text(r, bytes)))
                        .filter(|r| !r.is_empty());
                    if !name.is_empty() {
                        let scope_vec = receiver.into_iter().collect::<Vec<_>>();
                        push_symbol(symbols, &scope_vec, name, "method", child, bytes);
                    }
                }
            }
            "type_declaration" => {
                let mut inner_cursor = child.walk();
                for spec in child.children(&mut inner_cursor) {
                    if spec.kind() == "type_spec" {
                        if let Some(name_node) = spec.child_by_field_name("name") {
                            let name = node_text(name_node, bytes).trim().to_string();
                            let kind = spec
                                .child_by_field_name("type")
                                .map(|t| match t.kind() {
                                    "struct_type" => "struct",
                                    "interface_type" => "interface",
                                    _ => "type",
                                })
                                .unwrap_or("type");
                            if !name.is_empty() {
                                push_symbol(symbols, &[], name, kind, spec, bytes);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(feature = "semantic-ast")]
fn extract_go_receiver_type(raw: &str) -> String {
    let inner = raw.trim().trim_start_matches('(').trim_end_matches(')').trim();
    inner
        .split_whitespace()
        .last()
        .unwrap_or("")
        .trim_start_matches('*')
        .split('[')
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ast_outline_and_read_symbol_rust_ts_python() {
        let rust_src = r#"
/// Computes greeting text for caller.
pub fn greet(name: &str) -> String {
    format!("hi {}", name)
}

pub struct Hub {
    pub count: usize,
}

impl Hub {
    /// Increment internal counter.
    pub fn bump(&mut self) -> usize {
        self.count += 1;
        self.count
    }
}
"#;
        let rs_syms = extract_ast_symbols("rs", rust_src).expect("Rust AST should parse");
        assert!(rs_syms.iter().any(|s| s.qualified_name == "greet" && s.kind == "fn"));
        assert!(rs_syms.iter().any(|s| s.qualified_name == "Hub" && s.kind == "struct"));
        assert!(rs_syms.iter().any(|s| s.qualified_name == "Hub::bump" && s.kind == "method" || (s.qualified_name == "Hub::bump" && s.kind == "fn")));

        let bump_body = read_symbol_from_content("src/lib.rs", "rs", rust_src, "Hub::bump", true, 200)
            .expect("should read Hub::bump");
        assert!(bump_body.contains("Increment internal counter."));
        assert!(bump_body.contains("self.count += 1;"));

        let ts_src = r#"
export interface Config {
  port: number;
}

export class Client {
  async connect(): Promise<void> {
    return;
  }
}

export const helper = (x: number): number => x * 2;
"#;
        let ts_syms = extract_ast_symbols("ts", ts_src).expect("TS AST should parse");
        assert!(ts_syms.iter().any(|s| s.qualified_name == "Config" && s.kind == "interface"));
        assert!(ts_syms.iter().any(|s| s.qualified_name == "Client::connect" && s.kind == "method"));
        assert!(ts_syms.iter().any(|s| s.qualified_name == "helper" && s.kind == "function"));
        let connect_body = read_symbol_from_content("client.ts", "ts", ts_src, "Client.connect", true, 200)
            .expect("dot notation should resolve Client::connect");
        assert!(connect_body.contains("async connect()"));

        let py_src = r#"
class Worker:
    # Run loop
    def execute(self, task_id: str) -> bool:
        return bool(task_id)
"#;
        let py_syms = extract_ast_symbols("py", py_src).expect("Python AST should parse");
        assert!(py_syms.iter().any(|s| s.qualified_name == "Worker" && s.kind == "class"));
        assert!(py_syms.iter().any(|s| s.qualified_name == "Worker::execute" && s.kind == "method"));
        let exec_body = read_symbol_from_content("worker.py", "py", py_src, "Worker.execute", true, 200)
            .expect("Python method should be readable");
        assert!(exec_body.contains("# Run loop"));
        assert!(exec_body.contains("return bool(task_id)"));
    }
}
