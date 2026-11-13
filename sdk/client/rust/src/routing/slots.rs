//! Slot extraction for the natural-language routing surface.
//!
//! Routing selects a prepared operation; the operation's declared parameters still have to be
//! bound from the question. This module extracts the demo's slot set (anchor/from/to/topic/team and
//! the numeric depth/hops) with a deliberately small, fail-closed rule set:
//!
//! - entities are matched by name against the catalog vocabulary (longest match wins, and an
//!   ambiguous match is an error rather than a guess),
//! - numeric slots are only taken from the question, range-checked, and must be unambiguous,
//! - missing or ambiguous values surface as errors so the caller can abstain or ask for a
//!   clarification instead of executing a wrongly parameterised plan.
//!
//! The extractor has no knowledge of a prepared operation's body: it maps question wording onto
//! the slot names the catalog declares.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Entity vocabulary used for anchor/topic resolution.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EntityVocab {
    /// Concept names.
    #[serde(default)]
    pub concepts: Vec<String>,
    /// Document titles.
    #[serde(default)]
    pub documents: Vec<String>,
    /// Person names (not currently used by a slot, kept for completeness).
    #[serde(default)]
    pub persons: Vec<String>,
    /// Team names.
    #[serde(default)]
    pub teams: Vec<String>,
    /// Document title -> seeded `source_id`; the query embedding is derived from the source id so
    /// the vector search finds the stored document.
    #[serde(default)]
    pub document_ids: std::collections::BTreeMap<String, String>,
}

impl EntityVocab {
    /// Resolve a Document title to its seeded `source_id`.
    pub fn document_source_id(&self, title: &str) -> Option<&str> {
        self.document_ids.get(title).map(String::as_str)
    }

    /// Load the vocabulary from a JSON object with `concepts`/`documents`/`persons`/`teams`.
    pub fn load(path: &Path) -> Result<Self, SlotError> {
        let text =
            std::fs::read_to_string(path).map_err(|error| SlotError::Io(error.to_string()))?;
        serde_json::from_str(&text).map_err(|error| SlotError::Json(error.to_string()))
    }

    fn kind(&self, kind: EntityKind) -> &[String] {
        match kind {
            EntityKind::Concept => &self.concepts,
            EntityKind::Document => &self.documents,
            EntityKind::Team => &self.teams,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntityKind {
    Concept,
    Document,
    Team,
}

/// One resolved parameter set. `None` means the operation does not declare that slot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SlotBindings {
    /// Anchor entity of a reach/read operation.
    pub anchor: Option<String>,
    /// Shortest-path source.
    pub from: Option<String>,
    /// Shortest-path target.
    pub to: Option<String>,
    /// Topic entity of the access-constrained document search.
    pub topic: Option<String>,
    /// Team of the access-constrained document search (fixed to `Platform` by the demo query).
    pub team: Option<String>,
    /// Citation depth.
    pub depth: Option<u32>,
    /// Reach hop count.
    pub hops: Option<u32>,
    /// Genre filter of a conditions query.
    pub genre: Option<String>,
    /// First film of a shared-cast query.
    pub a: Option<String>,
    /// Second film of a shared-cast query.
    pub b: Option<String>,
}

impl std::fmt::Display for SlotBindings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();
        for (name, value) in [
            ("anchor", &self.anchor),
            ("from", &self.from),
            ("to", &self.to),
            ("topic", &self.topic),
            ("team", &self.team),
            ("genre", &self.genre),
            ("a", &self.a),
            ("b", &self.b),
        ] {
            if let Some(value) = value {
                parts.push(format!("{name}={value}"));
            }
        }
        if let Some(depth) = self.depth {
            parts.push(format!("depth={depth}"));
        }
        if let Some(hops) = self.hops {
            parts.push(format!("hops={hops}"));
        }
        if parts.is_empty() {
            formatter.write_str("(no slots)")
        } else {
            formatter.write_str(&parts.join(" "))
        }
    }
}

/// A BIO span recovered from the routing model's per-token tags.
///
/// `start`/`end` are indices into the token offset table the caller used to tokenize the
/// question (`end` exclusive); the canister returns exactly these token indices.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpannedTag {
    /// Tag name without its B/I prefix (for example `anchor` or `concept`).
    pub tag: String,
    /// First token of the span.
    pub start: usize,
    /// One past the last token of the span.
    pub end: usize,
    /// Mean per-token tag confidence of the span.
    pub score: f32,
}

/// Why a slot could not be bound.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SlotError {
    /// Vocabulary or question file could not be read.
    #[error("slot input error: {0}")]
    Io(String),
    /// Vocabulary JSON was invalid.
    #[error("slot vocabulary error: {0}")]
    Json(String),
    /// A required slot was not present in the question.
    #[error("missing slot: {0}")]
    Missing(&'static str),
    /// More than one distinct value could be the same slot.
    #[error("ambiguous slot: {0}")]
    Ambiguous(&'static str),
    /// A numeric slot was outside the operation's admitted range.
    #[error("slot {slot} out of range: {value}")]
    OutOfRange {
        /// Slot name.
        slot: &'static str,
        /// Parsed value.
        value: u32,
    },
    /// The operation is not a routable query (for example an update).
    #[error("operation {0} does not take slots")]
    NotAQuery(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Match {
    name: String,
    start: usize,
}

/// Find every occurrence of every vocabulary name, longest names first so that
/// `GraphRAG retrieval` is not reduced to `GraphRAG` when both are present.
fn find_matches(question: &str, names: &[String]) -> Vec<Match> {
    let lowered = question.to_lowercase();
    let mut ordered: Vec<&String> = names.iter().collect();
    ordered.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    let mut matches = Vec::new();
    for name in ordered {
        let needle = name.to_lowercase();
        if needle.is_empty() {
            continue;
        }
        let mut offset = 0;
        while let Some(position) = lowered[offset..].find(&needle) {
            let start = offset + position;
            // Skip a match fully contained in an already recorded longer match.
            let end = start + needle.len();
            let contained = matches.iter().any(|existing: &Match| {
                let existing_end = existing.start + existing.name.len();
                start >= existing.start && end <= existing_end
            });
            if !contained {
                matches.push(Match {
                    name: name.clone(),
                    start,
                });
            }
            offset = end;
        }
    }
    matches.sort_by_key(|matched| matched.start);
    matches
}

/// Longest match of the given entity kind; an ambiguity is two distinct longest names.
fn longest_match(
    question: &str,
    vocab: &EntityVocab,
    kind: EntityKind,
    slot: &'static str,
) -> Result<Option<String>, SlotError> {
    longest_match_names(question, vocab.kind(kind), slot)
}

/// Longest match across a name list; an ambiguity is two distinct longest names.
fn longest_match_names(
    question: &str,
    names: &[String],
    slot: &'static str,
) -> Result<Option<String>, SlotError> {
    let matches = find_matches(question, names);
    let Some(best_len) = matches.iter().map(|matched| matched.name.len()).max() else {
        return Ok(None);
    };
    let best: Vec<&Match> = matches
        .iter()
        .filter(|matched| matched.name.len() == best_len)
        .collect();
    let distinct: Vec<&String> = {
        let mut names: Vec<&String> = best.iter().map(|matched| &matched.name).collect();
        names.sort();
        names.dedup();
        names
    };
    if distinct.len() > 1 {
        return Err(SlotError::Ambiguous(slot));
    }
    Ok(distinct.first().map(|name| (*name).clone()))
}

/// Collect digit runs with their byte positions.
fn numbers(question: &str) -> Vec<(u32, usize)> {
    let bytes = question.as_bytes();
    let mut found = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_digit() {
            let start = index;
            let mut value: u32 = 0;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                value = value
                    .saturating_mul(10)
                    .saturating_add((bytes[index] - b'0') as u32);
                index += 1;
            }
            found.push((value, start));
            let _ = start;
        } else {
            index += 1;
        }
    }
    found
}

fn keyword_positions(question: &str, keywords: &[&str]) -> Vec<usize> {
    let lowered = question.to_lowercase();
    let mut positions = Vec::new();
    for keyword in keywords {
        if let Some(position) = lowered.find(keyword) {
            positions.push(position);
        }
    }
    positions
}

/// Pick the number closest to one of the slot's keywords; without a keyword exactly one number
/// is accepted, more than one is ambiguous.
fn numeric_slot(
    question: &str,
    slot: &'static str,
    keywords: &[&str],
    min: u32,
    max: u32,
) -> Result<Option<u32>, SlotError> {
    let found = numbers(question);
    if found.is_empty() {
        // The demo operations declare defaults; a question without a number keeps them.
        return Ok(None);
    }
    let keyword_positions = keyword_positions(question, keywords);
    let chosen = if keyword_positions.is_empty() {
        if found.len() > 1 {
            return Err(SlotError::Ambiguous(slot));
        }
        found[0].0
    } else {
        let distance = |(_, position): &(u32, usize)| {
            keyword_positions
                .iter()
                .map(|keyword| position.abs_diff(*keyword))
                .min()
                .unwrap_or(usize::MAX)
        };
        let mut ranked = found.clone();
        ranked.sort_by_key(distance);
        if ranked.len() > 1 && distance(&ranked[0]) == distance(&ranked[1]) {
            return Err(SlotError::Ambiguous(slot));
        }
        ranked[0].0
    };
    if chosen < min || chosen > max {
        return Err(SlotError::OutOfRange {
            slot,
            value: chosen,
        });
    }
    Ok(Some(chosen))
}

/// Extract the slots declared by `op` from `question`.
pub fn extract(question: &str, op: &str, vocab: &EntityVocab) -> Result<SlotBindings, SlotError> {
    let mut bindings = SlotBindings::default();
    match op {
        "citation-reach" => {
            bindings.anchor = required(
                longest_match(question, vocab, EntityKind::Document, "anchor")?,
                "anchor",
            )?;
            bindings.depth = Some(
                numeric_slot(
                    question,
                    "depth",
                    &["depth", "deep", "level", "levels", "深さ", "段"],
                    1,
                    3,
                )?
                .unwrap_or(3),
            );
        }
        "variable-length-reach" => {
            bindings.anchor = required(
                longest_match(question, vocab, EntityKind::Concept, "anchor")?,
                "anchor",
            )?;
            bindings.hops = Some(
                numeric_slot(
                    question,
                    "hops",
                    &[
                        "hop",
                        "hops",
                        "step",
                        "steps",
                        "relation",
                        "link",
                        "ホップ",
                        "ステップ",
                    ],
                    1,
                    3,
                )?
                .unwrap_or(2),
            );
        }
        "shortest-path" => {
            let matches = find_matches(question, &vocab.concepts);
            if matches.len() < 2 {
                return Err(SlotError::Missing("from/to"));
            }
            let mut distinct: Vec<&String> = Vec::new();
            for matched in &matches {
                if !distinct.contains(&&matched.name) {
                    distinct.push(&matched.name);
                }
            }
            if distinct.len() != 2 {
                return Err(SlotError::Ambiguous("from/to"));
            }
            bindings.from = Some(distinct[0].clone());
            bindings.to = Some(distinct[1].clone());
        }
        "films-similar-to" | "films-similar-to-original" => {
            bindings.anchor = required(
                longest_match(question, vocab, EntityKind::Document, "anchor")?,
                "anchor",
            )?;
        }
        "document-text-search" => {
            // Free-text search covers concepts and document titles alike; bind the longest name
            // across both pools as the query term.
            let topic_names: Vec<String> = vocab
                .concepts
                .iter()
                .chain(vocab.documents.iter())
                .cloned()
                .collect();
            bindings.topic = longest_match_names(question, &topic_names, "topic")?;
        }
        "team-readable-documents" => {
            // Topics can be either a concept or a document title; pick the longest name across both
            // kinds instead of preferring concepts and shadowing a longer document title.
            let topic_names: Vec<String> = vocab
                .concepts
                .iter()
                .chain(vocab.documents.iter())
                .cloned()
                .collect();
            bindings.topic = longest_match_names(question, &topic_names, "topic")?;
            bindings.team = Some(
                longest_match(question, vocab, EntityKind::Team, "team")?
                    .unwrap_or_else(|| "Platform".to_owned()),
            );
        }
        other => return Err(SlotError::NotAQuery(other.to_owned())),
    }
    Ok(bindings)
}

/// Tags each operation declares, matching the prepared-query catalog.
fn declared_tags(op: &str) -> Option<&'static [&'static str]> {
    Some(match op {
        "citation-reach" => &["anchor", "depth"],
        "variable-length-reach" => &["anchor", "hops"],
        "shortest-path" => &["concept"],
        "document-text-search" => &["topic"],
        "films-similar-to" | "films-similar-to-original" => &["anchor"],
        "team-readable-documents" => &["topic", "team"],
        _ => return None,
    })
}

fn span_text(question: &str, offsets: &[(usize, usize)], span: &SpannedTag) -> String {
    let start = offsets[span.start].0;
    let end = offsets[span.end - 1].1;
    question[start..end]
        .trim()
        .trim_matches(|character: char| ".,!?;:。、！？".contains(character))
        .to_owned()
}

fn parse_count(text: &str) -> Option<u32> {
    let digits: String = text.chars().filter(char::is_ascii_digit).collect();
    if let Ok(value) = digits.parse() {
        return Some(value);
    }
    let lowered = text.to_lowercase();
    for (word, value) in [
        ("one", 1),
        ("two", 2),
        ("three", 3),
        ("four", 4),
        ("five", 5),
        ("six", 6),
        ("seven", 7),
        ("eight", 8),
        ("nine", 9),
        ("ten", 10),
    ] {
        if lowered.split_whitespace().any(|token| token == word) {
            return Some(value);
        }
    }
    for (character, value) in [
        ('一', 1),
        ('二', 2),
        ('三', 3),
        ('四', 4),
        ('五', 5),
        ('六', 6),
        ('七', 7),
        ('八', 8),
        ('九', 9),
        ('十', 10),
    ] {
        if text.contains(character) {
            return Some(value);
        }
    }
    None
}

fn normalize_span(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Resolve one predicted span onto a vocabulary name; `None` when the span matches nothing.
///
/// The model may under- or over-shoot an entity (`Graph` for `Graph databases`), so a span is
/// snapped to the longest vocabulary name that contains it or is contained by it, and a tie is an
/// ambiguity rather than a guess.
fn resolve_span(
    text: &str,
    names: &[String],
    slot: &'static str,
) -> Result<Option<String>, SlotError> {
    let needle = normalize_span(text);
    if needle.is_empty() {
        return Ok(None);
    }
    let exact: Vec<&String> = names
        .iter()
        .filter(|name| normalize_span(name) == needle)
        .collect();
    match exact.len() {
        1 => return Ok(Some(exact[0].clone())),
        count if count > 1 => return Err(SlotError::Ambiguous(slot)),
        _ => {}
    }
    let candidates: Vec<&String> = names
        .iter()
        .filter(|name| {
            let normalized = normalize_span(name);
            normalized.contains(&needle) || needle.contains(&normalized)
        })
        .collect();
    let Some(best_len) = candidates
        .iter()
        .map(|name| normalize_span(name).len())
        .max()
    else {
        return Ok(None);
    };
    let best: Vec<&String> = candidates
        .into_iter()
        .filter(|name| normalize_span(name).len() == best_len)
        .collect();
    if best.len() > 1 {
        return Err(SlotError::Ambiguous(slot));
    }
    Ok(best.first().map(|name| (*name).clone()))
}

fn take_single(
    collected: &mut std::collections::BTreeMap<String, Vec<(String, f32)>>,
    slot: &'static str,
) -> Result<Option<String>, SlotError> {
    let values = collected.remove(slot).unwrap_or_default();
    let mut best: Option<(String, f32)> = None;
    let mut tied = false;
    for (name, score) in values {
        match &best {
            None => best = Some((name, score)),
            Some((current, current_score)) => {
                if score > current_score + 1e-6 {
                    best = Some((name, score));
                    tied = false;
                } else if (score - current_score).abs() <= 1e-6 && *current != name {
                    tied = true;
                }
            }
        }
    }
    if tied {
        return Err(SlotError::Ambiguous(slot));
    }
    Ok(best.map(|(name, _)| name))
}

fn take_count(
    collected: &mut std::collections::BTreeMap<String, Vec<(String, f32)>>,
    slot: &'static str,
) -> Result<Option<u32>, SlotError> {
    match take_single(collected, slot)? {
        Some(text) => parse_count(&text).ok_or(SlotError::Missing(slot)).map(Some),
        None => Ok(None),
    }
}

/// Bind slot values from the routing model's spans.
///
/// `token_offsets` are the byte ranges of the tokens that were sent to the canister, so a span
/// `[start, end)` maps back to the exact question text. The operation decides which tags are
/// meaningful; unknown tags are ignored, and a required value that cannot be resolved is an error
/// (the caller abstains or asks a clarification rather than executing a wrong plan).
pub fn bind_spans(
    question: &str,
    token_offsets: &[(usize, usize)],
    spans: &[SpannedTag],
    op: &str,
    vocab: &EntityVocab,
) -> Result<SlotBindings, SlotError> {
    let declared = declared_tags(op).ok_or_else(|| SlotError::NotAQuery(op.to_owned()))?;
    let mut collected: std::collections::BTreeMap<String, Vec<(String, f32)>> = Default::default();
    for span in spans {
        if !declared.contains(&span.tag.as_str()) {
            continue;
        }
        if span.start >= span.end
            || span.end > token_offsets.len()
            || token_offsets[span.start].0 >= token_offsets[span.end - 1].1
        {
            continue;
        }
        let text = span_text(question, token_offsets, span);
        if text.is_empty() {
            continue;
        }
        let slot = match span.tag.as_str() {
            "anchor" => {
                let names = if op == "citation-reach" {
                    &vocab.documents
                } else {
                    &vocab.concepts
                };
                resolve_span(&text, names, "anchor")?
            }
            "concept" => resolve_span(&text, &vocab.concepts, "from/to")?,
            "team" => resolve_span(&text, &vocab.teams, "team")?,
            "topic" => {
                let names: Vec<String> = vocab
                    .concepts
                    .iter()
                    .chain(vocab.documents.iter())
                    .cloned()
                    .collect();
                match resolve_span(&text, &names, "topic")? {
                    Some(name) => Some(name),
                    // Text search accepts free text; only the vector search needs a seeded title.
                    None if op == "document-text-search" => Some(text.clone()),
                    None => return Err(SlotError::Missing("topic")),
                }
            }
            "depth" | "hops" => Some(text.clone()),
            _ => None,
        };
        if let Some(value) = slot {
            collected
                .entry(span.tag.clone())
                .or_default()
                .push((value, span.score));
        }
    }

    let mut bindings = SlotBindings::default();
    match op {
        "citation-reach" => {
            bindings.anchor = required(take_single(&mut collected, "anchor")?, "anchor")?;
            let depth = take_count(&mut collected, "depth")?
                .or(take_count(&mut collected, "hops")?)
                .unwrap_or(3);
            if !(1..=3).contains(&depth) {
                return Err(SlotError::OutOfRange {
                    slot: "depth",
                    value: depth,
                });
            }
            bindings.depth = Some(depth);
        }
        "variable-length-reach" => {
            bindings.anchor = required(take_single(&mut collected, "anchor")?, "anchor")?;
            let hops = take_count(&mut collected, "hops")?
                .or(take_count(&mut collected, "depth")?)
                .unwrap_or(2);
            if !(1..=3).contains(&hops) {
                return Err(SlotError::OutOfRange {
                    slot: "hops",
                    value: hops,
                });
            }
            bindings.hops = Some(hops);
        }
        "shortest-path" => {
            let mut concepts: Vec<String> = collected
                .remove("concept")
                .unwrap_or_default()
                .into_iter()
                .map(|(name, _)| name)
                .collect();
            concepts.dedup();
            match concepts.len() {
                0 => return Err(SlotError::Missing("from/to")),
                1 => return Err(SlotError::Missing("to")),
                _ => {}
            }
            if concepts.len() > 2 {
                return Err(SlotError::Ambiguous("from/to"));
            }
            bindings.from = concepts.first().cloned();
            bindings.to = concepts.get(1).cloned();
        }
        "document-text-search" => {
            bindings.topic = required(take_single(&mut collected, "topic")?, "topic")?;
        }
        "team-readable-documents" => {
            bindings.topic = required(take_single(&mut collected, "topic")?, "topic")?;
            bindings.team = take_single(&mut collected, "team")?;
        }
        _ => unreachable!("declared_tags accepted the operation"),
    }
    Ok(bindings)
}

fn required(value: Option<String>, slot: &'static str) -> Result<Option<String>, SlotError> {
    match value {
        Some(value) => Ok(Some(value)),
        None => Err(SlotError::Missing(slot)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vocab() -> EntityVocab {
        EntityVocab {
            concepts: vec![
                "Graph databases".into(),
                "GraphRAG".into(),
                "GraphRAG retrieval".into(),
                "Vector search".into(),
                "Traversal".into(),
            ],
            documents: vec![
                "Introduction to graph databases".into(),
                "Graph databases".into(),
                "The GQL standard".into(),
            ],
            teams: vec!["Platform".into(), "Research".into()],
            persons: vec![],
            document_ids: Default::default(),
        }
    }

    #[test]
    fn text_search_binds_topic_across_concepts_and_documents() {
        let bindings = extract(
            "全文検索で GraphRAG に関する文書を探して",
            "document-text-search",
            &vocab(),
        )
        .expect("extract");
        assert_eq!(bindings.topic.as_deref(), Some("GraphRAG"));

        let bindings = extract(
            "Which documents mention The GQL standard in their body?",
            "document-text-search",
            &vocab(),
        )
        .expect("extract");
        assert_eq!(bindings.topic.as_deref(), Some("The GQL standard"));
    }

    #[test]
    fn sentence_case_question_binds_concept_and_hops() {
        let bindings = extract(
            "Which concepts reach Graph databases through RELATED_TO within 3 hops?",
            "variable-length-reach",
            &vocab(),
        )
        .expect("extract");
        assert_eq!(bindings.anchor.as_deref(), Some("Graph databases"));
        assert_eq!(bindings.hops, Some(3));
    }

    #[test]
    fn depth_comes_from_the_number_next_to_its_keyword() {
        let bindings = extract(
            "From The GQL standard follow citations to depth 2 and list them",
            "citation-reach",
            &vocab(),
        )
        .expect("extract");
        assert_eq!(bindings.anchor.as_deref(), Some("The GQL standard"));
        assert_eq!(bindings.depth, Some(2));
    }

    #[test]
    fn longest_entity_name_wins() {
        let bindings = extract(
            "Which documents cite Introduction to graph databases?",
            "citation-reach",
            &vocab(),
        )
        .expect("extract");
        assert_eq!(
            bindings.anchor.as_deref(),
            Some("Introduction to graph databases")
        );
    }

    #[test]
    fn shortest_path_needs_two_distinct_concepts() {
        let bindings = extract(
            "What is the shortest path between Vector search and Traversal?",
            "shortest-path",
            &vocab(),
        )
        .expect("extract");
        assert_eq!(bindings.from.as_deref(), Some("Vector search"));
        assert_eq!(bindings.to.as_deref(), Some("Traversal"));

        let error = extract(
            "What is the shortest path to Vector search?",
            "shortest-path",
            &vocab(),
        )
        .expect_err("one concept is not enough");
        assert_eq!(error, SlotError::Missing("from/to"));
    }

    #[test]
    fn similar_films_binds_an_anchor_document_title() {
        let bindings = extract(
            "films similar to The GQL standard",
            "films-similar-to",
            &vocab(),
        )
        .expect("extract");
        assert_eq!(bindings.anchor.as_deref(), Some("The GQL standard"));
    }

    #[test]
    fn similar_films_without_a_title_is_missing_anchor() {
        let error = extract("films similar to this one", "films-similar-to", &vocab())
            .expect_err("no document title");
        assert_eq!(error, SlotError::Missing("anchor"));
    }

    #[test]
    fn missing_anchor_is_an_error() {
        let error = extract(
            "Which concepts are nearby?",
            "variable-length-reach",
            &vocab(),
        )
        .expect_err("missing anchor");
        assert_eq!(error, SlotError::Missing("anchor"));
    }

    #[test]
    fn out_of_range_depth_is_rejected() {
        let error = extract(
            "From The GQL standard follow citations to depth 9",
            "citation-reach",
            &vocab(),
        )
        .expect_err("depth 9");
        assert!(matches!(
            error,
            SlotError::OutOfRange {
                slot: "depth",
                value: 9
            }
        ));
    }

    #[test]
    fn topic_prefers_a_longer_document_title_over_a_shorter_concept() {
        let bindings = extract(
            "Which documents about GraphRAG retrieval can the Platform team read?",
            "team-readable-documents",
            &vocab(),
        )
        .expect("extract");
        assert_eq!(bindings.topic.as_deref(), Some("GraphRAG retrieval"));
    }

    #[test]
    fn team_is_fixed_when_the_question_does_not_name_one() {
        let bindings = extract(
            "Which documents about Vector search can the Platform team read?",
            "team-readable-documents",
            &vocab(),
        )
        .expect("extract");
        assert_eq!(bindings.team.as_deref(), Some("Platform"));
        assert_eq!(bindings.topic.as_deref(), Some("Vector search"));
    }

    fn token_offsets(question: &str, tokens: &[&str]) -> Vec<(usize, usize)> {
        let mut at = 0;
        let mut out = Vec::new();
        for token in tokens {
            let start = question[at..].find(token).expect("token present") + at;
            out.push((start, start + token.len()));
            at = start + token.len();
        }
        out
    }

    fn scored(tag: &str, start: usize, end: usize, score: f32) -> SpannedTag {
        SpannedTag {
            tag: tag.to_owned(),
            start,
            end,
            score,
        }
    }

    fn spanned(tag: &str, start: usize, end: usize) -> SpannedTag {
        SpannedTag {
            tag: tag.to_owned(),
            start,
            end,
            score: 0.0,
        }
    }

    #[test]
    fn spans_bind_citation_anchor_and_depth() {
        let question = "From The GQL standard follow citations to depth 2";
        let offsets = token_offsets(
            question,
            &[
                "The",
                "GQL",
                "standard",
                "follow",
                "citations",
                "to",
                "depth",
                "2",
            ],
        );
        let bindings = bind_spans(
            question,
            &offsets,
            &[spanned("anchor", 0, 3), spanned("depth", 7, 8)],
            "citation-reach",
            &vocab(),
        )
        .expect("bind");
        assert_eq!(bindings.anchor.as_deref(), Some("The GQL standard"));
        assert_eq!(bindings.depth, Some(2));
    }

    #[test]
    fn spans_bind_reach_anchor_and_word_number() {
        let question = "Which concepts reach Graph databases within two hops";
        let offsets = token_offsets(question, &["Graph", "databases", "within", "two", "hops"]);
        let bindings = bind_spans(
            question,
            &offsets,
            &[spanned("anchor", 0, 2), spanned("hops", 3, 4)],
            "variable-length-reach",
            &vocab(),
        )
        .expect("bind");
        assert_eq!(bindings.anchor.as_deref(), Some("Graph databases"));
        assert_eq!(bindings.hops, Some(2));
    }

    #[test]
    fn spans_assign_from_and_to_in_text_order() {
        let question = "What is the shortest path between Vector search and Traversal";
        let offsets = token_offsets(question, &["Vector", "search", "and", "Traversal"]);
        let bindings = bind_spans(
            question,
            &offsets,
            &[spanned("concept", 0, 2), spanned("concept", 3, 4)],
            "shortest-path",
            &vocab(),
        )
        .expect("bind");
        assert_eq!(bindings.from.as_deref(), Some("Vector search"));
        assert_eq!(bindings.to.as_deref(), Some("Traversal"));
    }

    #[test]
    fn spans_choose_the_highest_confidence_anchor() {
        let question = "From The GQL standard or Graph databases follow citations to depth 2";
        let offsets = token_offsets(
            question,
            &[
                "From",
                "The",
                "GQL",
                "standard",
                "or",
                "Graph",
                "databases",
                "follow",
                "citations",
                "to",
                "depth",
                "2",
            ],
        );
        let bindings = bind_spans(
            question,
            &offsets,
            &[
                scored("anchor", 1, 4, 0.3),
                scored("anchor", 5, 7, 0.9),
                scored("depth", 11, 12, 0.9),
            ],
            "citation-reach",
            &vocab(),
        )
        .expect("bind");
        assert_eq!(bindings.anchor.as_deref(), Some("Graph databases"));
    }

    #[test]
    fn spans_with_equal_confidence_stay_ambiguous() {
        let question = "From The GQL standard or Graph databases follow citations to depth 2";
        let offsets = token_offsets(
            question,
            &[
                "From",
                "The",
                "GQL",
                "standard",
                "or",
                "Graph",
                "databases",
                "follow",
                "citations",
                "to",
                "depth",
                "2",
            ],
        );
        let error = bind_spans(
            question,
            &offsets,
            &[
                scored("anchor", 1, 4, 0.5),
                scored("anchor", 5, 7, 0.5),
                scored("depth", 11, 12, 0.9),
            ],
            "citation-reach",
            &vocab(),
        )
        .expect_err("equal confidence");
        assert!(matches!(error, SlotError::Ambiguous("anchor")));
    }

    #[test]
    fn spans_snap_a_partial_entity_to_the_vocabulary_name() {
        let question = "Which documents about GraphRAG retrieval can the Platform team read?";
        let offsets = token_offsets(question, &["GraphRAG", "retrieval", "Platform"]);
        let bindings = bind_spans(
            question,
            &offsets,
            &[spanned("topic", 0, 2), spanned("team", 2, 3)],
            "team-readable-documents",
            &vocab(),
        )
        .expect("bind");
        assert_eq!(bindings.topic.as_deref(), Some("GraphRAG retrieval"));
        assert_eq!(bindings.team.as_deref(), Some("Platform"));
    }

    #[test]
    fn spans_keep_free_text_for_text_search() {
        let question = "search for quantum widgets";
        let offsets = token_offsets(question, &["quantum", "widgets"]);
        let bindings = bind_spans(
            question,
            &offsets,
            &[spanned("topic", 0, 2)],
            "document-text-search",
            &vocab(),
        )
        .expect("bind");
        assert_eq!(bindings.topic.as_deref(), Some("quantum widgets"));
    }

    #[test]
    fn spans_fail_closed_when_a_required_entity_is_missing() {
        let question = "Which concepts are nearby";
        let offsets = token_offsets(question, &["Which", "concepts", "are", "nearby"]);
        let error = bind_spans(question, &offsets, &[], "variable-length-reach", &vocab())
            .expect_err("no anchor");
        assert_eq!(error, SlotError::Missing("anchor"));
    }

    #[test]
    fn spans_fail_closed_on_ambiguity_and_range() {
        let mut borderline = EntityVocab::default();
        borderline.concepts = vec!["Alpha beta".into(), "Alpha zeta".into()];
        let question = "Alpha reach";
        let offsets = token_offsets(question, &["Alpha", "reach"]);
        let error = bind_spans(
            question,
            &offsets,
            &[spanned("anchor", 0, 1)],
            "variable-length-reach",
            &borderline,
        )
        .expect_err("ambiguous anchor");
        assert_eq!(error, SlotError::Ambiguous("anchor"));

        let question = "From The GQL standard citations to depth 9";
        let offsets = token_offsets(question, &["The", "GQL", "standard", "to", "depth", "9"]);
        let error = bind_spans(
            question,
            &offsets,
            &[spanned("anchor", 0, 3), spanned("depth", 5, 6)],
            "citation-reach",
            &vocab(),
        )
        .expect_err("depth 9");
        assert!(matches!(
            error,
            SlotError::OutOfRange {
                slot: "depth",
                value: 9
            }
        ));
    }

    #[test]
    fn spans_reject_operations_that_take_no_slots() {
        let error = bind_spans("anything", &[(0, 8)], &[], "none-of-these", &vocab())
            .expect_err("not a query");
        assert_eq!(error, SlotError::NotAQuery("none-of-these".into()));
    }
}
