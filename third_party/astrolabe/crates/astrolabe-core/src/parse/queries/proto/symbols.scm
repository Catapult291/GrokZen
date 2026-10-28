;; Protobuf symbols (tree-sitter-proto 0.6).
;; Mirrors upstream tags.scm: package / message / enum / service / rpc /
;; enum constants. Field names are omitted in v1 (follow-up).

(package
  (full_ident) @name) @definition.module

(message
  (message_name
    (identifier) @name)) @definition.class

(group
  (message_name
    (identifier) @name)) @definition.class

(enum
  (enum_name
    (identifier) @name)) @definition.enum

(enum_field
  (identifier) @name) @definition.constant

(service
  (service_name
    (identifier) @name)) @definition.interface

(rpc
  (rpc_name
    (identifier) @name)) @definition.method
