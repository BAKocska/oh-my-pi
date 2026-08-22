//! Secret-rule validation and deterministic masking primitives.

/// Streaming placeholder-boundary withholding.
/// Built-in provider credential rules.
pub mod builtins;
/// Recursive model-authored JSON transforms.
pub mod json;
/// Author-sensitive message transform policy.
pub mod message;
/// Atomic bidirectional text transform.
pub mod obfuscator;
/// Keyed reversible placeholder grammar and registration.
pub mod placeholder;
/// Dedicated redaction-only projection.
pub mod redact;
/// Deterministic and regex-safe irreversible replacements.
pub mod replacement;
/// Closed secret declaration contract and regex validation.
pub mod rule;
/// Placeholder-boundary buffering for streamed provider output.
pub mod stream;
/// Origin-aware fixed-point text transforms.
pub mod tracked;
