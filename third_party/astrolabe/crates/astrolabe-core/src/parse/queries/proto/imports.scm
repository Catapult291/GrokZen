;; Protobuf imports (tree-sitter-proto 0.6).
;;
;; `import "path/to/x.proto";` / `import public "…";` / `import weak "…";`
;; Capture the whole `string` node (quotes included); extract_imports runs
;; `unquote`, same as languages without a string_content child.

(import
  path: (string) @import)
