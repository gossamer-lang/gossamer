# `std::validate`

Status: experimental

Trait-based field validation: implement Validate, collect FieldErrors into Errors.

## Items

| Item | Signature | Description |
|---|---|---|
| `Validate` | `trait Validate` | Implement on a struct to declare field-level validation rules. |
| `FieldError` | `type FieldError` | One field-scoped validation failure: dotted path, message, optional code. |
| `Errors` | `type Errors` | Aggregated FieldError set, indexable by dotted path. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
