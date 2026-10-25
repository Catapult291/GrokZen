;; C# symbol definitions (tree-sitter-c-sharp 0.23).
;;
;; `@definition.<kind>` on the defining node, `@name` on its identifier.
;; Records are classes (a `record struct` is still reported as a class; there
;; is no separate symbol kind). Properties are type members (`field`).
;; Constructors are methods, matching the Java query pack. Namespaces,
;; including file-scoped `namespace N;`, are modules.

(class_declaration
  name: (identifier) @name) @definition.class

(struct_declaration
  name: (identifier) @name) @definition.struct

(interface_declaration
  name: (identifier) @name) @definition.interface

(enum_declaration
  name: (identifier) @name) @definition.enum

(record_declaration
  name: (identifier) @name) @definition.class

(method_declaration
  name: (identifier) @name) @definition.method

(constructor_declaration
  name: (identifier) @name) @definition.method

(property_declaration
  name: (identifier) @name) @definition.field

(namespace_declaration
  name: (_) @name) @definition.module

(file_scoped_namespace_declaration
  name: (_) @name) @definition.module

;; Destructors (`~Foo()`) keep the type name, like constructors.
(destructor_declaration
  name: (identifier) @name) @definition.method

;; Local functions at any depth: top-level statements, scripts, and nested
;; helpers inside methods / accessors / lambdas.
(local_function_statement
  name: (identifier) @name) @definition.function

;; ------------------------------------------------------------ type members
;; Fields: one match per declarator (`int a, b;`). `const` fields are
;; reported as Const by the extractor (it inspects the `modifier` children,
;; which a query predicate cannot do without per-modifier duplicate matches).
(field_declaration
  (variable_declaration
    (variable_declarator
      name: (identifier) @name))) @definition.field

;; `public event EventHandler Changed, Closed;`
(event_field_declaration
  (variable_declaration
    (variable_declarator
      name: (identifier) @name))) @definition.field

;; `public event EventHandler Changed { add; remove; }`
(event_declaration
  name: (identifier) @name) @definition.field

;; Indexers are named `this`, as in C# language servers.
(indexer_declaration
  "this" @name) @definition.method

;; `public static Foo operator +(Foo a, Foo b)`; the extractor renders the
;; name as `operator +`.
(operator_declaration
  operator: _ @name) @definition.method

;; `public static implicit operator string(Foo f)`; rendered as
;; `implicit operator string` by the extractor.
(conversion_operator_declaration
  type: (_) @name) @definition.method

(enum_member_declaration
  name: (identifier) @name) @definition.const

;; `public delegate void Handler(object sender);`
(delegate_declaration
  name: (identifier) @name) @definition.type
