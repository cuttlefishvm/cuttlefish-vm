//! signature: {results_path: text, failures_path: text, succeeded: json, failed: json} -> {results_path: text, candidates: number, admitted: bool}
//! A successful transport job must not hide rejected fan-out items.
//! Export/promotion must still check the independent domain validator and review.
if input.failed != 0 || input.succeeded == 0 { throw "candidate batch is incomplete"; }
#{results_path: input.results_path, candidates: input.succeeded, admitted: false}
