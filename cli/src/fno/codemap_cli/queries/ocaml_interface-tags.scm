; Modules
;--------

(
  (comment)? @doc .
  (module_definition
    (module_binding (module_name) @name.definition.module) @definition.module
  )
  (#strip! @doc "^\\(\\*+\\s*|\\s*\\*+\\)$")
)

(module_path (module_name) @name.reference.module) @reference.module
(extended_module_path (module_name) @name.reference.module) @reference.module

(
  (comment)? @doc .
  (module_type_definition (module_type_name) @name.definition.interface) @definition.interface
  (#strip! @doc "^\\(\\*+\\s*|\\s*\\*+\\)$")
)

(module_type_path (module_type_name) @name.reference.implementation) @reference.implementation


; Classes
;--------

(
  (comment)? @doc .
  [
    (class_definition
      (class_binding (class_name) @name.definition.class) @definition.class
    )
    (class_type_definition
      (class_type_binding (class_type_name) @name.definition.class) @definition.class
    )
  ]
  (#strip! @doc "^\\(\\*+\\s*|\\s*\\*+\\)$")
)

[
  (class_path (class_name) @name.reference.class)
  (class_type_path (class_type_name) @name.reference.class)
] @reference.class

(
  (comment)? @doc .
  (method_definition (method_name) @name.definition.method) @definition.method
  (#strip! @doc "^\\(\\*+\\s*|\\s*\\*+\\)$")
)

(method_invocation (method_name) @name.reference.call) @reference.call


; Types
;------

(
  (comment)? @doc .
  (type_definition
    (type_binding
      name: [
        (type_constructor) @name.definition.type
        (type_constructor_path (type_constructor) @name.definition.type)
      ]
    ) @definition.type
  )
  (#strip! @doc "^\\(\\*+\\s*|\\s*\\*+\\)$")
)

(type_constructor_path (type_constructor) @name.reference.type) @reference.type

[
  (constructor_declaration (constructor_name) @name.definition.enum_variant)
  (tag_specification (tag) @name.definition.enum_variant)
] @definition.enum_variant

[
  (constructor_path (constructor_name) @name.reference.enum_variant)
  (tag) @name.reference.enum_variant
] @reference.enum_variant

(field_declaration (field_name) @name.definition.field) @definition.field

(field_path (field_name) @name.reference.field) @reference.field

(
  (comment)? @doc .
  (external (value_name) @name.definition.function) @definition.function
  (#strip! @doc "^\\(\\*+\\s*|\\s*\\*+\\)$")
)

(
  (comment)? @doc .
  (value_specification
    (value_name) @name.definition.function
  ) @definition.function
  (#strip! @doc "^\\(\\*+\\s*|\\s*\\*+\\)$")
)
