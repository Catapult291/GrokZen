//! Dart extraction hooks. The symbols query tags every `function_signature`
//! as a function; inside a class / mixin / extension body it is a method.

use crate::types::SymbolKind;

/// `kind` for a `@definition.function` match on a `function_signature`:
/// members (`class_member > method_signature | declaration`) are methods.
pub(super) fn refine_kind(definition: tree_sitter::Node<'_>, kind: SymbolKind) -> SymbolKind {
    if kind != SymbolKind::Function || definition.kind() != "function_signature" {
        return kind;
    }
    // function_signature > method_signature > method_declaration >
    // class_member (or declaration > class_member for abstract methods).
    let mut cur = definition.parent();
    while let Some(p) = cur {
        match p.kind() {
            "class_member" => return SymbolKind::Method,
            "method_signature" | "method_declaration" | "declaration" => cur = p.parent(),
            _ => break,
        }
    }
    kind
}
