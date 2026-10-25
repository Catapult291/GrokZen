(class_declaration name: (identifier) @name) @definition.class
(enum_declaration name: (identifier) @name) @definition.enum
(mixin_declaration name: (identifier) @name) @definition.trait
(function_signature name: (identifier) @name) @definition.function
(getter_signature name: (identifier) @name) @definition.method
(setter_signature name: (identifier) @name) @definition.method
;; Constructors are methods, matching the Java / C# packs. Every
;; constructor (unnamed, named, `const`, `factory`, redirecting) is indexed
;; under its class name, so `find_symbol Foo` lands on all construction sites a
;; class rename has to touch. A named constructor `Foo.bar(...)` is indexed a
;; second time as `bar` (one match per `name:` identifier).
(constructor_signature name: (identifier) @name) @definition.method
(constant_constructor_signature name: (identifier) @name) @definition.method
(factory_constructor_signature name: (identifier) @name) @definition.method
(redirecting_factory_constructor_signature name: (identifier) @name) @definition.method
;; `operator +`, `operator []`, `operator ==`: the extractor renders the name.
(operator_signature) @definition.method
;; `class D = B with M;`
(mixin_application_class (identifier) @name) @definition.class
(extension_declaration name: (identifier) @name) @definition.class
(extension_type_declaration name: (extension_type_name (identifier) @name)) @definition.class
;; `typedef Cb = void Function();` and legacy `typedef void Cb();`
(type_alias (type_identifier) @name) @definition.type
(enum_constant name: (identifier) @name) @definition.field
;; Class / mixin / extension fields: `final int x, y;`, `static const k = 1;`,
;; `abstract int af;`.
(declaration (initialized_identifier_list (initialized_identifier name: (identifier) @name))) @definition.field
(declaration (static_final_declaration_list (static_final_declaration name: (identifier) @name))) @definition.field
(declaration (identifier_list (identifier) @name)) @definition.field
;; Top-level variables and constants.
(top_level_variable_declaration (initialized_identifier_list (initialized_identifier name: (identifier) @name))) @definition.variable
(top_level_variable_declaration (static_final_declaration_list (static_final_declaration name: (identifier) @name))) @definition.variable
