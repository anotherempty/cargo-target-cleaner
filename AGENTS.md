# Repository instructions

## Communication

- Use concise, direct language. Preserve technical accuracy, constraints, and important reasoning.
- Avoid filler, repetition, and unnecessary detail.
- Use numbered lists when presenting multiple items.
- The user has the final decision on all project matters. Follow their decisions rather than substituting your preferences.
- Warn only when you believe the user's decision is mistaken; explain the specific concern briefly, then respect their final decision.

## Development stage

- This project is in early development, not a released library with existing users or production datasets.
- Change APIs and storage formats directly as implementation evolves.
- Do not add backward-compatibility layers, migration tools, legacy-format tests, or migration warnings unless the user explicitly requests them.
- Do not assume prototype code or temporary test repositories require compatibility or migration support.

## Rust

### Dependencies

- Declare all dependencies in root `Cargo.toml` under `[workspace.dependencies]`.
- Member crates must inherit dependencies using `workspace = true`, including development and build dependencies.
- Do not declare dependency versions, sources, or paths directly in member crates.

### Type organization

- Keep each type and its implementations together: type definition, trait implementations, then inherent implementations.
- Do not place unrelated types between a type definition and its implementations.

### Modules

- Prefer a module directory with `mod.rs` when multiple files belong to that module.
- Use a single `module_name.rs` file when the module needs only one file.
- Place out-of-line module declarations (`mod name;`) at the top of the file, after file-level configuration and attributes. Keep declaration-specific attributes attached to their declarations.
- Place inline module definitions (`mod name { ... }`) at the bottom of the file, including test modules.

### Errors and panics

- Do not use `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!`, or other deliberate panic paths outside tests.
- Avoid operations that can panic on unchecked input, including unchecked indexing.
- Assertions and panic-based checks are permitted in tests only.
- Use concrete error types with meaningful variants and typed context.
- Propagate recoverable errors using `Result` and `?`; do not silently discard errors.
- Do not erase errors into strings or general-purpose boxed error types.

### Domain types

- Use `String` and `str` for text only.
- Represent identifiers, states, actions, positions, quantities, paths, and other non-text concepts with appropriate concrete types, enums, structs, or newtypes.
- A newtype must use storage appropriate to its domain; wrapping a `String` does not make encoded non-text data a proper domain model.
- Store fixed-length binary values, such as hashes, in fixed-size byte arrays with named length constants. Represent supported formats or lengths with explicit enum variants when needed; do not use `String` or variable-length buffers for fixed-length values.
- Decode hexadecimal, Base64, and other wire encodings at input boundaries; encode only at output boundaries. Do not retain encoded text as the internal source of truth for binary values.
- Before adding or changing a domain type, verify that its underlying representation expresses known size, range, and variant constraints, rather than relying solely on runtime validation of a generic container.
- Parse and validate external text at boundaries; keep internal domain values typed.
- Put parsing, validation, normalization, and invariant enforcement in the domain type that owns the value. For example, endpoint URL rules belong in the endpoint structure, not configuration or client code.
- Enforce those rules through constructors, conversions, and deserialization. Keep representation private and ensure public operations preserve invariants; callers must not repeat the type's validation.

### Whole-type review

- Whenever adding or changing a type, review the complete type and affected callers before reporting completion; do not stop at the individual issue the user identified.
- Check storage representation, size and range constraints, construction and mutation invariants, parsing and serialization boundaries, error behavior, trait implementations, and ownership semantics together.
- Reevaluate trait derives after representation changes. Derive `Copy` for small value types whose fields support it and whose semantics permit unrestricted duplication; do not apply it to resource ownership or secret-bearing types merely because their fields allow it. Assess `Clone`, `Eq`, `Ord`, and `Hash` for domain-correct semantics rather than retaining or adding them mechanically.
- Update affected callers in the same change, removing redundant clones, allocations, conversions, and duplicated validation made obsolete by the new model.
- Verify affected behavior with appropriate tests and lint checks, and manually inspect constraints those checks cannot prove. Passing compilation alone is not a complete review.
- Resolve related omissions within the changed type and its affected usage without waiting for the user to identify each one. Report unrelated findings separately rather than expanding into an unsolicited repository-wide rewrite.

### Constants and values

- Use named constants for reused fixed values of any type and meaningful numeric values, including in tests.
- Keep constants with their owning type or module; name them by purpose and units. Keep unrelated meanings separate and document non-obvious values.
- Use existing typed constants or enums for protocol values and typed configuration fields for configurable policies.

### Ownership

- Prefer borrowing through references when ownership does not need to change.
- Consume values when transferring ownership or expressing a state transition.
- Avoid unnecessary cloning and shared ownership.

### Design

- Prefer object-oriented design using structs, enums, traits, and associated methods over functional programming patterns.
- Associate behavior with the type or domain concept responsible for it.
- Use free functions only when behavior cannot reasonably belong to an object-oriented concept. Language-required entry points such as `main` are exempt.

### Diagnostics

- Use `tracing` for diagnostics and logging.
- Do not use `print!`, `println!`, `eprint!`, `eprintln!`, or `dbg!`.
- Keep protocol output separate from diagnostics; send tracing output to stderr when stdout carries MCP traffic.
