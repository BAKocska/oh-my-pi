//! Versioned constrained-decoding grammars for edit dialects.

/// Formal hashline input grammar.
pub const HASHLINE: &str = include_str!("../grammars/hashline.lark");
/// Codex apply-patch envelope grammar.
pub const APPLY_PATCH: &str = include_str!("../grammars/apply_patch.lark");
/// Sloppy section/match/rewrite grammar.
pub const SLOPPY: &str = include_str!("../grammars/sloppy.lark");
