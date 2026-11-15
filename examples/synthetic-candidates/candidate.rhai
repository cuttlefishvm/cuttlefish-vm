//! signature: json -> {parent_id: text, book_id: text, split: text, patient: text, required_weeks: number, question: text, documents: json, reference_evidence: json, requires_review: bool}
//! A small candidate-generation example, not an admitted training dataset.
//! Run only against frozen training inputs. Keep parent and book groups in one
//! split. The model supplies wording; deterministic code preserves facts and
//! derives reference evidence. Free prose still needs independent review.
//! Wrong-patient and stale-document traps here are deterministic mutations,
//! not examples of newly generated clinical facts.
if input.split != "train" { throw "only training parents may generate candidates"; }
if input.parent_id == "" || input.book_id == "" { throw "missing grouping identity"; }
if input.required_weeks < 0 { throw "invalid threshold"; }
let response = parse_json(infer(
    "Return JSON with one field, question. Write one short generic question asking " +
    "which current documents for the requested patient support the required " +
    "therapy duration. Do not add names, numbers, diagnoses, answers, or claims.", 256));
if type_of(response.question) != "string" || response.question.len() < 10 || response.question.len() > 300 {
    throw "question must contain 10 to 300 characters";
}
// The generator never decides its own gold label. These fields come only from
// the frozen packet; model-provided facts, IDs and labels are never consumed.
let reference = [];
let ids = [];
for document in input.documents {
    if ids.contains(document.id) { throw "duplicate document id"; }
    ids.push(document.id);
    if document.weeks < 0 { throw "invalid document duration"; }
    if document.patient == input.patient && document.current && document.weeks >= input.required_weeks {
        reference.push(document.id);
    }
}
if reference.len() == 0 { throw "fixture needs qualifying reference evidence"; }
#{parent_id: input.parent_id, book_id: input.book_id, split: input.split,
  patient: input.patient, required_weeks: input.required_weeks,
  question: response.question, documents: input.documents,
  reference_evidence: reference, requires_review: true}
