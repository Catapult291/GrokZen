;; Objective-C call sites.
;;
;; `@reference.call` on the call node, `@name` on the callee's identifier.
;; Message sends capture their first selector keyword; `parse/objc.rs::call_name`
;; rebuilds the full selector (`setObject:forKey:`) from the message node.

(call_expression
  function: (identifier) @name) @reference.call

(call_expression
  function: (field_expression
    field: (field_identifier) @name)) @reference.call

(message_expression
  receiver: (_)
  .
  method: (identifier) @name) @reference.call
