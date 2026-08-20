//! Consolidated omp-tools integration harness; one binary per crate keeps
//! static links off the per-file hot path.

mod documents;
mod edit;
mod eval;
mod glob;
mod grep;
mod markit_doc;
mod markit_odf;
mod markit_odp;
mod markit_ods;
mod markit_ooxml;
mod markit_ppt;
mod markit_xls;
mod markit_xlsx;
mod pdf;
mod read;
mod shell;
mod web;
mod write;
