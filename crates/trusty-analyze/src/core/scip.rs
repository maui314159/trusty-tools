//! SCIP (Scalable and Precise Index for Code) ingestion.
//!
//! Why: tree-sitter adapters give us cheap, language-agnostic *structural*
//! knowledge, but they don't resolve cross-file references. SCIP indexes
//! emitted by precise indexers (`rust-analyzer`, `scip-typescript`,
//! `scip-python`, `scip-java`, ...) carry fully-resolved symbols and
//! relationships. Letting users POST a SCIP index lets the analyzer surface
//! precise call/implementation graphs without re-implementing each language's
//! semantic frontend.
//!
//! What: a single entry point, `extract_kg_from_scip`, decodes a SCIP
//! `Index` protobuf and walks every document, emitting a language-neutral
//! [`KgGraph`]. Each `SymbolInformation` with a definition occurrence becomes
//! a [`KgNode`]; SCIP `Relationship` records (`is_implementation`) become
//! `Implements` edges; non-definition occurrences become `References` edges
//! from the enclosing symbol to the referenced symbol.
//!
//! Test: see the module-level tests — they build a small `Index` in memory,
//! round-trip it through protobuf, and assert nodes/edges come back out.

use crate::types::{KgEdge, KgEdgeKind, KgGraph, KgNode, KgNodeKind};
use anyhow::{Context, Result};
use protobuf::Message;
use scip::types::{symbol_information::Kind as ScipKind, Index, Occurrence, SymbolInformation};

/// Outcome of a SCIP ingest, suitable for JSON serialization in API responses.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScipIngestSummary {
    /// Number of `Document` entries in the SCIP index.
    pub documents: usize,
    /// Total occurrences across all documents.
    pub occurrences: usize,
    /// `SymbolInformation` entries with a resolvable definition.
    pub symbols: usize,
    /// Nodes that were added to the knowledge graph.
    pub kg_nodes: usize,
    /// Edges that were added to the knowledge graph.
    pub kg_edges: usize,
}

/// Decode a SCIP protobuf payload into a `(graph, summary)` pair.
///
/// Why: separates protobuf decoding from graph construction so callers can
/// surface a useful "what just happened" response without re-walking the
/// graph. Returns `Err` only when the payload itself is malformed.
///
/// What: parses an `Index` message, then delegates to `index_to_graph` for
/// the actual translation. Counts are taken from the parsed `Index`, not the
/// resulting graph, so unsupported symbol kinds still contribute to the
/// `symbols` total even when they don't end up as `KgNode`s.
///
/// Test: `decode_smoke_index_produces_expected_graph` round-trips a hand-built
/// index through the decoder.
pub fn extract_kg_from_scip(bytes: &[u8]) -> Result<(KgGraph, ScipIngestSummary)> {
    let index = Index::parse_from_bytes(bytes).context("failed to parse SCIP Index protobuf")?;
    Ok(index_to_graph(&index))
}

/// Convert an already-parsed SCIP `Index` into a `KgGraph` + summary.
///
/// Why: exposed separately so tests can build an `Index` programmatically and
/// drive the converter without going through `write_to_bytes`. The HTTP
/// handler always goes through `extract_kg_from_scip`.
///
/// What: walks each document's `symbols` to create nodes, then its
/// `occurrences` to create reference edges back to the enclosing definition
/// symbol when one is known.
///
/// Test: `index_to_graph_emits_implementation_edges` and
/// `index_to_graph_emits_reference_edges` cover both edge kinds.
pub fn index_to_graph(index: &Index) -> (KgGraph, ScipIngestSummary) {
    let mut graph = KgGraph::default();
    let mut summary = ScipIngestSummary {
        documents: index.documents.len(),
        occurrences: 0,
        symbols: 0,
        kg_nodes: 0,
        kg_edges: 0,
    };

    for doc in &index.documents {
        summary.occurrences += doc.occurrences.len();
        summary.symbols += doc.symbols.len();

        let language = if doc.language.is_empty() {
            language_from_path(&doc.relative_path)
        } else {
            doc.language.to_ascii_lowercase()
        };

        // 1) Materialize nodes for every SymbolInformation in this document.
        //    SCIP doesn't necessarily attach a line range to SymbolInformation
        //    directly; we find the matching *definition* occurrence (if any)
        //    to populate start_line/end_line.
        for sym in &doc.symbols {
            let range = find_definition_range(&doc.occurrences, &sym.symbol);
            if let Some(node) = symbol_to_node(sym, &language, &doc.relative_path, range) {
                graph.nodes.push(node);
            }
            // Translate SCIP relationships → KgEdges. SCIP's
            // `is_implementation` covers "Type implements Trait / class
            // implements Interface", which maps cleanly to our `Implements`
            // edge taxonomy.
            for rel in &sym.relationships {
                if rel.is_implementation {
                    graph.edges.push(KgEdge {
                        from: scip_symbol_id(&sym.symbol, &language),
                        to: scip_symbol_id(&rel.symbol, &language),
                        kind: KgEdgeKind::Implements,
                        weight: 1.0,
                    });
                }
            }
        }

        // 2) Reference edges: every non-definition occurrence becomes a
        //    References edge from the document's enclosing definition (if we
        //    can guess one) to the referenced symbol. We pick the most
        //    recently-seen definition symbol within the same document as a
        //    best-effort enclosing scope; precise enclosing requires
        //    walking ranges, which is fine to defer.
        let mut current_enclosing: Option<String> = None;
        for occ in &doc.occurrences {
            if is_definition(occ) {
                current_enclosing = Some(scip_symbol_id(&occ.symbol, &language));
                continue;
            }
            if let Some(from) = current_enclosing.as_ref() {
                if occ.symbol.is_empty() {
                    continue;
                }
                let to = scip_symbol_id(&occ.symbol, &language);
                if &to == from {
                    continue;
                }
                graph.edges.push(KgEdge {
                    from: from.clone(),
                    to,
                    kind: KgEdgeKind::References,
                    weight: 1.0,
                });
            }
        }
    }

    summary.kg_nodes = graph.nodes.len();
    summary.kg_edges = graph.edges.len();
    (graph, summary)
}

/// True when the occurrence's `symbol_roles` bitmask has the SCIP
/// `Definition` bit (1 << 0) set.
///
/// Why: SCIP encodes "this is the definition site of the symbol" as a bit
/// flag rather than a separate field; without it we'd treat every occurrence
/// as a reference.
/// What: bit-tests against the well-known constant `1`.
/// Test: `is_definition_detects_role_bit`.
fn is_definition(occ: &Occurrence) -> bool {
    // SCIP `SymbolRole::Definition` is bit 0 (value 1). See
    // <https://github.com/sourcegraph/scip/blob/main/scip.proto>.
    (occ.symbol_roles & 0x1) != 0
}

/// Return the line range of the definition-occurrence for `symbol` in
/// `occurrences`, or `(0, 0)` if none exists.
fn find_definition_range(occurrences: &[Occurrence], symbol: &str) -> (u32, u32) {
    for occ in occurrences {
        if occ.symbol == symbol && is_definition(occ) {
            // SCIP ranges are [start_line, start_char, end_line?, end_char]
            // or [start_line, start_char, end_char] (3 values when the symbol
            // is single-line). Both encodings put `start_line` at index 0 and
            // `end_line` at index 2 when present.
            let start = occ.range.first().copied().unwrap_or(0).max(0) as u32;
            let end = if occ.range.len() >= 4 {
                occ.range[2].max(0) as u32
            } else {
                start
            };
            return (start, end);
        }
    }
    (0, 0)
}

/// Build a `KgNode` from a SCIP `SymbolInformation`. Returns `None` when the
/// SCIP kind is `UnspecifiedKind` (we don't want noisy unknown nodes).
fn symbol_to_node(
    sym: &SymbolInformation,
    language: &str,
    file: &str,
    range: (u32, u32),
) -> Option<KgNode> {
    let kind = map_scip_kind(sym.kind.enum_value().unwrap_or(ScipKind::UnspecifiedKind))?;
    let name = if !sym.display_name.is_empty() {
        sym.display_name.clone()
    } else {
        // Fall back to the last identifier-ish slice of the symbol string.
        sym.symbol
            .rsplit(['/', '.', '#', '(', ')'])
            .find(|s| !s.is_empty())
            .unwrap_or(&sym.symbol)
            .to_string()
    };
    let doc_comment = if sym.documentation.is_empty() {
        None
    } else {
        Some(sym.documentation.join("\n"))
    };
    let qualified_name = sym.symbol.clone();
    Some(KgNode {
        id: scip_symbol_id(&sym.symbol, language),
        kind,
        name,
        qualified_name,
        language: language.to_string(),
        file: file.to_string(),
        start_line: range.0,
        end_line: range.1,
        doc_comment,
        is_public: true, // SCIP only emits resolvable symbols; treat as public by default.
        extra: serde_json::json!({ "source": "scip" }),
    })
}

/// Map SCIP's enormous `Kind` enum to our compact `KgNodeKind` taxonomy.
///
/// Why: SCIP distinguishes 70+ symbol kinds (AbstractMethod, Accessor,
/// AssociatedType, ...); our knowledge graph only has a dozen. We pick the
/// closest match so downstream consumers don't have to learn SCIP.
/// Test: covered indirectly by the round-trip tests.
fn map_scip_kind(k: ScipKind) -> Option<KgNodeKind> {
    use ScipKind::*;
    Some(match k {
        UnspecifiedKind => return None,
        Class | Struct | Enum | Object | Type | TypeAlias | TypeParameter | Trait | Union => {
            KgNodeKind::Class
        }
        Interface | Protocol => KgNodeKind::Interface,
        Function | StaticMethod | Macro | Constructor | Operator => KgNodeKind::Function,
        Method | AbstractMethod | Accessor | Getter | Setter | SingletonMethod => {
            KgNodeKind::Method
        }
        Field | EnumMember | Constant | StaticField | Property | SingletonClass => {
            KgNodeKind::Field
        }
        Module | Namespace | SelfParameter | Package => KgNodeKind::Module,
        File => KgNodeKind::File,
        _ => return None,
    })
}

/// Construct the canonical KG id used to refer to a SCIP symbol.
///
/// Why: SCIP symbol strings already uniquely identify a symbol across the
/// whole index. Embedding the language in front matches the
/// `{language}:{kind}:{...}` convention used by the tree-sitter adapters, so
/// merged graphs deduplicate cleanly.
fn scip_symbol_id(symbol: &str, language: &str) -> String {
    format!("scip:{language}:{symbol}")
}

/// Cheap last-resort language guess from a relative path extension. Only used
/// when the SCIP `Document.language` field is empty (older indexers).
fn language_from_path(path: &str) -> String {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "rs" => "rust",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" => "javascript",
        "py" => "python",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        _ => "unknown",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::EnumOrUnknown;
    use scip::types::{Document, Index, Occurrence, Relationship, Symbol, SymbolInformation};

    fn occurrence(symbol: &str, definition: bool) -> Occurrence {
        let mut occ = Occurrence::new();
        occ.symbol = symbol.to_string();
        occ.symbol_roles = if definition { 0x1 } else { 0 };
        occ.range = vec![10, 0, 20]; // start_line=10, start_char=0, end_char=20
        occ
    }

    fn sym_info(symbol: &str, kind: ScipKind, display: &str) -> SymbolInformation {
        let mut s = SymbolInformation::new();
        s.symbol = symbol.to_string();
        s.kind = EnumOrUnknown::new(kind);
        s.display_name = display.to_string();
        s
    }

    #[test]
    fn is_definition_detects_role_bit() {
        let def = occurrence("scip-rust . . my_fn().", true);
        let refr = occurrence("scip-rust . . my_fn().", false);
        assert!(is_definition(&def));
        assert!(!is_definition(&refr));
    }

    #[test]
    fn index_to_graph_emits_nodes_for_function_and_class() {
        let mut doc = Document::new();
        doc.relative_path = "src/lib.rs".into();
        doc.language = "rust".into();
        doc.symbols.push(sym_info(
            "rust-analyzer . . hello().",
            ScipKind::Function,
            "hello",
        ));
        doc.symbols
            .push(sym_info("rust-analyzer . . Foo#", ScipKind::Class, "Foo"));
        doc.occurrences
            .push(occurrence("rust-analyzer . . hello().", true));
        doc.occurrences
            .push(occurrence("rust-analyzer . . Foo#", true));

        let mut index = Index::new();
        index.documents.push(doc);

        let (graph, summary) = index_to_graph(&index);
        assert_eq!(summary.documents, 1);
        assert!(summary.kg_nodes >= 2, "graph: {graph:?}");
        let kinds: Vec<&KgNodeKind> = graph.nodes.iter().map(|n| &n.kind).collect();
        assert!(kinds.iter().any(|k| matches!(k, KgNodeKind::Function)));
        assert!(kinds.iter().any(|k| matches!(k, KgNodeKind::Class)));
        // Every node should be tagged with the language.
        assert!(graph.nodes.iter().all(|n| n.language == "rust"));
    }

    #[test]
    fn index_to_graph_emits_implementation_edges() {
        let mut foo = sym_info("rust . . Foo#", ScipKind::Class, "Foo");
        let mut rel = Relationship::new();
        rel.symbol = "rust . . Bar#".into();
        rel.is_implementation = true;
        foo.relationships.push(rel);

        let bar = sym_info("rust . . Bar#", ScipKind::Interface, "Bar");

        let mut doc = Document::new();
        doc.relative_path = "src/lib.rs".into();
        doc.language = "rust".into();
        doc.symbols.push(foo);
        doc.symbols.push(bar);

        let mut index = Index::new();
        index.documents.push(doc);

        let (graph, _) = index_to_graph(&index);
        let impls: Vec<_> = graph
            .edges
            .iter()
            .filter(|e| e.kind == KgEdgeKind::Implements)
            .collect();
        assert_eq!(impls.len(), 1, "edges: {:?}", graph.edges);
        assert!(impls[0].from.contains("Foo"));
        assert!(impls[0].to.contains("Bar"));
    }

    #[test]
    fn index_to_graph_emits_reference_edges() {
        let mut doc = Document::new();
        doc.relative_path = "src/lib.rs".into();
        doc.language = "rust".into();
        doc.symbols
            .push(sym_info("rust . . caller().", ScipKind::Function, "caller"));
        doc.symbols
            .push(sym_info("rust . . callee().", ScipKind::Function, "callee"));
        // Definition of caller, then a reference inside it to callee.
        doc.occurrences.push(occurrence("rust . . caller().", true));
        doc.occurrences
            .push(occurrence("rust . . callee().", false));

        let mut index = Index::new();
        index.documents.push(doc);

        let (graph, _) = index_to_graph(&index);
        let refs: Vec<_> = graph
            .edges
            .iter()
            .filter(|e| e.kind == KgEdgeKind::References)
            .collect();
        assert_eq!(refs.len(), 1, "edges: {:?}", graph.edges);
        assert!(refs[0].from.contains("caller"));
        assert!(refs[0].to.contains("callee"));
    }

    #[test]
    fn decode_smoke_index_produces_expected_graph() {
        // Build a tiny Index, write to protobuf, then decode through the
        // public entry point. This is the end-to-end happy path.
        let mut doc = Document::new();
        doc.relative_path = "src/lib.rs".into();
        doc.language = "rust".into();
        doc.symbols
            .push(sym_info("rust . . hello().", ScipKind::Function, "hello"));
        doc.occurrences.push(occurrence("rust . . hello().", true));

        let mut index = Index::new();
        index.documents.push(doc);

        let bytes = index.write_to_bytes().expect("encode");
        let (graph, summary) = extract_kg_from_scip(&bytes).expect("decode");
        assert_eq!(summary.documents, 1);
        assert_eq!(graph.node_count(), 1);
        assert_eq!(graph.nodes[0].name, "hello");
    }

    #[test]
    fn decode_rejects_garbage_bytes() {
        let err = extract_kg_from_scip(&[0xFF, 0xFF, 0xFF, 0xFF]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("SCIP") || msg.contains("protobuf") || msg.contains("Index"),
            "unexpected error message: {msg}"
        );
    }

    #[test]
    fn language_from_path_handles_common_extensions() {
        assert_eq!(language_from_path("a.rs"), "rust");
        assert_eq!(language_from_path("a/b.ts"), "typescript");
        assert_eq!(language_from_path("c.py"), "python");
        assert_eq!(language_from_path("noext"), "unknown");
    }

    // Reference to unused import in tests; silences a warning if Symbol isn't
    // touched by future test additions.
    #[allow(dead_code)]
    fn _touch_symbol() -> Symbol {
        Symbol::new()
    }
}
