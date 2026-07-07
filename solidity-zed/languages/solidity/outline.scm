(contract_declaration
  "contract" @context
  name: (identifier) @name) @item

(interface_declaration
  "interface" @context
  name: (identifier) @name) @item

(library_declaration
  "library" @context
  name: (identifier) @name) @item

(struct_declaration
  "struct" @context
  name: (identifier) @name) @item

(enum_declaration
  "enum" @context
  name: (identifier) @name) @item

(function_definition
  "function" @context
  name: (identifier) @name) @item

(modifier_definition
  "modifier" @context
  name: (identifier) @name) @item

(event_definition
  "event" @context
  name: (identifier) @name) @item

(error_declaration
  "error" @context
  name: (identifier) @name) @item
