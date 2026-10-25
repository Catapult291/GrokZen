;; Rust import specifiers (tree-sitter-rust 0.24).
;;
;; `@import` — the `use` path exactly as written, visibility and trailing `;`
;; excluded, `crate::` / `self::` / `super::` prefixes preserved:
;;   use std::collections::HashMap;          -> std::collections::HashMap
;;   use crate::types::Language;             -> crate::types::Language
;;   use super::parent;                      -> super::parent
;;   use self::child::Thing as Other;        -> self::child::Thing
;;   pub use std::io::*;                     -> std::io::*
;;   use std::collections::{HashMap, HashSet}; -> std::collections::{HashMap, HashSet}
;;   extern crate alloc;                     -> alloc
;;
;; Brace groups are handed over *verbatim*, not expanded. A query can only
;; capture nodes, never concatenate text, so expansion has to happen in Rust
;; code anyway; and the leading path segments, which are what a module
;; resolver keys on, are already contiguous in the raw text. The resolver
;; should split on the first `{` and treat everything before `::{` as the
;; common prefix, then fan out over the comma-separated (possibly nested)
;; entries. Nested `as` aliases inside a group stay in the text.
;;
;; `@import.mod` — `mod foo;` declarations without a body. These are the actual
;; file-to-file links in a Rust crate (`foo.rs` / `foo/mod.rs`); emitted under
;; a separate capture so consumers opt in.

;; `use path as alias;` — strip the alias, keep the path.
(use_declaration
  argument: (use_as_clause
    path: (_) @import))

;; Every other top-level `use` argument shape.
(use_declaration
  argument: [
    (identifier)
    (scoped_identifier)
    (scoped_use_list)
    (use_list)
    (use_wildcard)
    (crate)
    (self)
    (super)
    (metavariable)
  ] @import)

;; `extern crate foo;` / `extern crate foo as bar;`
(extern_crate_declaration
  name: (identifier) @import)

;; `mod foo;`
(mod_item
  name: (identifier) @import.mod
  !body)
