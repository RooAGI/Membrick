use crate::index::{Claim, ClaimEvidence, ClaimStatus, DocRecord, SectionChunk};
use crate::reasoning_rules::reasoning_rules;
use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Clone, Default)]
pub struct ExtractedClaims {
    pub claims: Vec<Claim>,
}

pub trait ClaimExtractor {
    fn extract(&self, record: &DocRecord) -> ExtractedClaims;
    fn name(&self) -> &'static str;
}

#[derive(Debug, Clone, Default)]
pub struct ConservativeClaimExtractor;

/// Native event relationships are always built as part of the standard
/// record pipeline. This stays separate from optional generic claim scoring.
pub(crate) fn extract_native_event_claims(record: &DocRecord) -> Vec<Claim> {
    let mut claims = Vec::new();
    for chunk in &record.section_chunks {
        let mut event_claims = extract_event_relations(&chunk.content);
        event_claims.extend(extract_acquisition_relations(&chunk.content));
        event_claims.extend(personal_claims_for_chunk(record, chunk));
        let timestamp = chunk.timestamp.as_ref().or(record.timestamp.as_ref());
        if let Some(timestamp) = timestamp {
            let event_subjects: Vec<String> = event_claims
                .iter()
                .filter(|claim| claim.predicate == "participated_in")
                .map(|claim| claim.object.clone())
                .collect();
            for event in event_subjects {
                event_claims.push(relation(&event, "event_time", timestamp, 0.7));
            }
        }
        for claim in &mut event_claims {
            if claim.evidence.is_none() {
                claim.evidence = Some(claim_evidence(record, chunk, &chunk.content, None, None));
            }
        }
        claims.extend(event_claims);
    }
    dedupe_claims(&mut claims);
    claims
}

impl ConservativeClaimExtractor {
    fn from_existing_claims(record: &DocRecord, out: &mut Vec<Claim>) {
        out.extend(
            record
                .top_claims
                .iter()
                .filter(|claim| is_valid_claim(claim))
                .cloned(),
        );
    }

    fn from_chunks(record: &DocRecord, out: &mut Vec<Claim>) {
        for chunk in &record.section_chunks {
            out.extend(chunk_claims(record, chunk));
        }
    }
}

impl ClaimExtractor for ConservativeClaimExtractor {
    fn extract(&self, record: &DocRecord) -> ExtractedClaims {
        let mut claims = Vec::new();
        Self::from_existing_claims(record, &mut claims);
        Self::from_chunks(record, &mut claims);
        dedupe_claims(&mut claims);
        ExtractedClaims { claims }
    }

    fn name(&self) -> &'static str {
        "conservative"
    }
}

fn chunk_claims(record: &DocRecord, chunk: &SectionChunk) -> Vec<Claim> {
    let mut out = Vec::new();

    out.extend(extract_event_relations(&chunk.content));
    out.extend(extract_acquisition_relations(&chunk.content));
    out.extend(personal_claims_for_chunk(record, chunk));

    if let Some(topic) = record
        .probable_topic
        .as_ref()
        .filter(|s| !s.trim().is_empty())
    {
        if chunk.heading.to_lowercase().contains(&topic.to_lowercase())
            || chunk.content.to_lowercase().contains(&topic.to_lowercase())
        {
            out.push(Claim {
                subject: topic.clone(),
                predicate: "mentions".to_string(),
                object: chunk.heading.clone(),
                confidence: 0.55,
                ..Default::default()
            });
        }
    }

    if let Some(ts) = chunk.timestamp.as_ref().or(record.timestamp.as_ref()) {
        if !ts.trim().is_empty() {
            let event_subjects: Vec<String> = out
                .iter()
                .filter(|claim| claim.predicate == "participated_in")
                .map(|claim| claim.object.clone())
                .collect();
            for event in event_subjects {
                out.push(relation(&event, "event_time", ts, 0.7));
            }
            out.push(Claim {
                subject: chunk.heading.clone(),
                predicate: "timestamp".to_string(),
                object: ts.clone(),
                confidence: 0.5,
                ..Default::default()
            });
        }
    }

    for ent in &chunk.key_entities {
        out.push(Claim {
            subject: chunk.heading.clone(),
            predicate: "mentions_entity".to_string(),
            object: ent.clone(),
            confidence: 0.45,
            ..Default::default()
        });
    }

    for term in &chunk.important_terms {
        out.push(Claim {
            subject: chunk.heading.clone(),
            predicate: "important_term".to_string(),
            object: term.clone(),
            confidence: 0.4,
            ..Default::default()
        });
    }

    for claim in &mut out {
        if claim.evidence.is_none() {
            claim.evidence = Some(claim_evidence(record, chunk, &chunk.content, None, None));
        }
    }

    out
}

fn personal_claims_for_chunk(record: &DocRecord, chunk: &SectionChunk) -> Vec<Claim> {
    extract_personal_claim_spans(&chunk.content)
        .into_iter()
        .map(|span| {
            let mut claim = span.claim;
            claim.evidence = Some(claim_evidence(
                record,
                chunk,
                &chunk.content[span.start_byte..span.end_byte],
                Some(span.start_byte),
                Some(span.end_byte),
            ));
            claim
        })
        .collect()
}

fn claim_evidence(
    record: &DocRecord,
    chunk: &SectionChunk,
    text: &str,
    start_byte: Option<usize>,
    end_byte: Option<usize>,
) -> ClaimEvidence {
    ClaimEvidence {
        source_document_id: record.doc_id.clone(),
        source: record.source.clone(),
        source_turn_id: record.filters.get("turn_id").cloned(),
        source_chunk_id: Some(chunk.chunk_id.clone()),
        text: text.to_string(),
        start_byte,
        end_byte,
    }
}

/// One conservative personal-fact assertion grounded in a byte range of its
/// source text. This feeds both the index's existing `Claim` model and the
/// semantic supersession layer, so those paths cannot silently develop
/// different claim vocabularies.
#[derive(Debug, Clone)]
pub(crate) struct ClaimSpan {
    pub claim: Claim,
    pub start_byte: usize,
    pub end_byte: usize,
}

/// Extract profile and location relations used by the generic claim pipeline.
/// Rules are intentionally explicit and fail closed: unknown language stays
/// searchable as source text without being treated as a state-changing fact.
pub(crate) fn extract_personal_claim_spans(text: &str) -> Vec<ClaimSpan> {
    static RESIDENCE: OnceLock<Regex> = OnceLock::new();
    static MOVE: OnceLock<Regex> = OnceLock::new();
    static FAVORITE: OnceLock<Regex> = OnceLock::new();
    static FOOD_PREFERENCE: OnceLock<Regex> = OnceLock::new();
    static DIETARY: OnceLock<Regex> = OnceLock::new();
    static WALK: OnceLock<Regex> = OnceLock::new();
    static NO_REGULAR_PLACE: OnceLock<Regex> = OnceLock::new();

    let residence = RESIDENCE.get_or_init(|| {
        Regex::new(r"(?i)\b(?:i(?:['’]?m)?|we(?:['’]?re)?)\s+(?:currently\s+)?(?:live|living)\s+in\s+(?P<city>[^,.;!?]+?)(?:,\s+in\s+(?P<neighborhood>[^.;!?]+))?(?:[.!?]|$)")
            .expect("valid residence rule")
    });
    let moved = MOVE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(?:i(?:['’]?m)?|we(?:['’]?re)?)\s+moved\s+to\s+(?P<city>[^,.;!?]+)(?:[.!?]|$)",
        )
        .expect("valid move rule")
    });
    let favorite = FAVORITE.get_or_init(|| {
        Regex::new(r"(?i)\bmy\s+favo[u]?rite\s+place\s+to\s+eat\s+is\s+(?P<place>[^.;!?]+)")
            .expect("valid favorite restaurant rule")
    });
    let food_preference = FOOD_PREFERENCE.get_or_init(|| {
        Regex::new(r"(?i)\btheir\s+(?P<food>[^.;!?]+?)\s+(?:is|are)\s+(?:the\s+)?best\b")
            .expect("valid food preference rule")
    });
    let dietary = DIETARY.get_or_init(|| {
        Regex::new(r"(?i)\b(?:i['’]?m|i\s+am|we['’]?re|we\s+are)\s+(?P<diet>vegetarian|vegan|pescatarian)\b")
            .expect("valid dietary rule")
    });
    let walk = WALK.get_or_init(|| {
        Regex::new(r"(?i)\b(?:a\s+)?(?P<minutes>\d+|one|two|three|four|five|ten|fifteen|twenty)[- ]minute\s+walk\s+from\s+(?:my|our)\s+flat\b")
            .expect("valid home-walk rule")
    });
    let no_regular_place = NO_REGULAR_PLACE.get_or_init(|| {
        Regex::new(r"(?i)\bhaven['’]?t\s+found\s+a\s+regular\s+place\s+to\s+eat\s+yet\b")
            .expect("valid no-regular-place rule")
    });

    let mut output = Vec::new();
    let mut city = None::<String>;
    let mut neighborhood = None::<String>;
    let mut restaurant = None::<String>;
    for (start, end, sentence) in sentence_spans(text) {
        if let Some(captures) = residence.captures(sentence) {
            let city_value = clean_profile_value(captures.name("city").unwrap().as_str());
            if !city_value.is_empty() {
                let predicate = if city.is_some() && captures.name("neighborhood").is_none() {
                    neighborhood = Some(city_value.clone());
                    "resides_in_neighborhood"
                } else {
                    city = Some(city_value.clone());
                    "resides_in_city"
                };
                output.push(profile_claim(
                    "user",
                    predicate,
                    &city_value,
                    None,
                    start,
                    end,
                ));
            }
            if let Some(value) = captures.name("neighborhood") {
                let neighborhood_value = clean_profile_value(value.as_str());
                if !neighborhood_value.is_empty() {
                    neighborhood = Some(neighborhood_value.clone());
                    output.push(profile_claim(
                        "user",
                        "resides_in_neighborhood",
                        &neighborhood_value,
                        None,
                        start,
                        end,
                    ));
                }
            }
        } else if let Some(captures) = moved.captures(sentence) {
            let city_value = clean_profile_value(captures.name("city").unwrap().as_str());
            if !city_value.is_empty() {
                city = Some(city_value.clone());
                output.push(profile_claim(
                    "user",
                    "resides_in_city",
                    &city_value,
                    None,
                    start,
                    end,
                ));
            }
        }

        if let Some(captures) = favorite.captures(sentence) {
            let value = clean_profile_value(captures.name("place").unwrap().as_str());
            if !value.is_empty() {
                restaurant = Some(value.clone());
                output.push(profile_claim(
                    "user",
                    "favorite_restaurant",
                    &value,
                    None,
                    start,
                    end,
                ));
            }
        }
        if let Some(captures) = food_preference.captures(sentence) {
            let value = clean_profile_value(captures.name("food").unwrap().as_str());
            if !value.is_empty() {
                output.push(profile_claim(
                    "user",
                    "likes_food",
                    &value,
                    None,
                    start,
                    end,
                ));
            }
        }
        if let Some(captures) = dietary.captures(sentence) {
            output.push(profile_claim(
                "user",
                "dietary_preference",
                &captures.name("diet").unwrap().as_str().to_lowercase(),
                None,
                start,
                end,
            ));
        }
        if let Some(captures) = walk.captures(sentence) {
            if let (Some(place), Some(area)) = (restaurant.as_deref(), neighborhood.as_deref()) {
                let minutes = captures.name("minutes").unwrap().as_str().to_lowercase();
                output.push(profile_claim(
                    place,
                    "walk_minutes_from_residence",
                    &format!("{minutes} minutes; residence={area}"),
                    None,
                    start,
                    end,
                ));
            }
        }
        if no_regular_place.is_match(sentence) {
            if let Some(area) = neighborhood.as_deref().or(city.as_deref()) {
                output.push(profile_claim(
                    "user",
                    "has_regular_restaurant_in",
                    area,
                    Some(ClaimStatus::Negated),
                    start,
                    end,
                ));
            }
        }
    }
    output
}

fn profile_claim(
    subject: &str,
    predicate: &str,
    object: &str,
    status: Option<ClaimStatus>,
    start_byte: usize,
    end_byte: usize,
) -> ClaimSpan {
    ClaimSpan {
        claim: Claim {
            subject: subject.to_string(),
            predicate: predicate.to_string(),
            object: object.to_string(),
            confidence: 0.9,
            status,
            event_time: None,
            evidence: None,
        },
        start_byte,
        end_byte,
    }
}

fn clean_profile_value(value: &str) -> String {
    let mut value = value
        .trim()
        .trim_matches(|ch: char| ch.is_ascii_punctuation() || ch.is_whitespace())
        .to_string();
    for suffix in [" now", " last month", " this month", " recently"] {
        if value.to_lowercase().ends_with(suffix) {
            value.truncate(value.len() - suffix.len());
            break;
        }
    }
    value.trim().to_lowercase()
}

fn sentence_spans(text: &str) -> Vec<(usize, usize, &str)> {
    let mut spans = Vec::new();
    let mut start = 0;
    for (index, ch) in text.char_indices() {
        if matches!(ch, '.' | '!' | '?' | '\n') {
            let end = index + ch.len_utf8();
            let leading = text[start..index].len() - text[start..index].trim_start().len();
            let trailing = text[start..index].trim_end().len();
            let span_start = start + leading;
            let span_end = start + trailing;
            if span_start < span_end {
                spans.push((span_start, end, &text[span_start..end]));
            }
            start = end;
        }
    }
    if start < text.len() {
        let leading = text[start..].len() - text[start..].trim_start().len();
        let span_start = start + leading;
        if span_start < text.len() {
            spans.push((span_start, text.len(), &text[span_start..]));
        }
    }
    spans
}

/// Extract a small set of high-confidence event relations from first-person
/// event clauses. This uses Membrick's native clause rules and the existing
/// `Claim` model; it does not depend on a provider or an external NLP runtime.
/// The rule is deliberately conservative: it only emits relations when a
/// first-person role/action is connected to a recognizable event noun.
fn extract_event_relations(text: &str) -> Vec<Claim> {
    let mut out = Vec::new();
    for sentence in text.split(['.', '!', '?', '\n']) {
        let sentence_lower = sentence.to_lowercase();
        let rules = reasoning_rules();
        let first_person = rules.first_person_words.iter().any(|marker| {
            sentence_lower.contains(format!(" {marker} ").as_str())
                || sentence_lower
                    .trim_start()
                    .starts_with(format!("{marker} ").as_str())
        });
        if !first_person {
            continue;
        }

        // Support both role-bearing and direct event frames. These are
        // clause patterns, not provider labels: the same relations are built
        // whether or not an enrichment provider is enabled.
        let sentence_lower = sentence_lower.as_str();
        let (role, event_start, prefer_named_event, status) =
            if let Some(as_pos) = sentence_lower.find(" as ") {
                let after_as = as_pos + 4;
                let Some(at_rel) = sentence_lower[after_as..].find(" at ") else {
                    continue;
                };
                let role = sentence[after_as..after_as + at_rel]
                    .trim()
                    .trim_start_matches("a ")
                    .trim_start_matches("an ")
                    .trim();
                (Some(role), after_as + at_rel + 4, false, None)
            } else {
                let Some(frame) = rules
                    .event_frames
                    .iter()
                    .find(|rule| sentence_lower.contains(&rule.phrase))
                else {
                    continue;
                };
                let frame_start = sentence_lower.find(&frame.phrase).unwrap_or_default();
                if !is_first_person_action_prefix(&sentence_lower[..frame_start]) {
                    continue;
                }
                let start = frame_start + frame.phrase.len();
                (
                    None,
                    start,
                    frame.prefer_named_event,
                    frame.status.as_deref().and_then(ClaimStatus::parse),
                )
            };
        let mut tail = sentence[event_start..]
            .split([',', ';'])
            .next()
            .unwrap_or_default();
        if prefer_named_event {
            if let Some(at_pos) = tail.to_lowercase().find(" at ") {
                let named_event = tail[at_pos + 4..].trim_start();
                if named_event.split_whitespace().any(|word| {
                    is_event_noun(
                        word.trim_matches(|c: char| !c.is_alphanumeric())
                            .to_lowercase()
                            .as_str(),
                    )
                }) {
                    tail = named_event;
                }
            }
        }
        // Keep only the noun phrase through its event head. This prevents a
        // following explanation ("... and it got me thinking ...") or a
        // trailing date phrase from becoming part of the event's identity.
        let words: Vec<&str> = tail.split_whitespace().collect();
        let Some(event_end) = words.iter().position(|word| {
            is_event_noun(
                word.trim_matches(|c: char| !c.is_alphanumeric())
                    .to_lowercase()
                    .as_str(),
            )
        }) else {
            continue;
        };
        let event_phrase_owned = words[..=event_end]
            .join(" ")
            .trim_start_matches("my ")
            .trim_start_matches("our ")
            .trim_start_matches("the ")
            .trim_start_matches("a ")
            .trim_start_matches("an ")
            .to_string();
        let event_phrase = event_phrase_owned.trim();
        let event_head = words[event_end].trim_matches(|c: char| !c.is_alphanumeric());
        if event_phrase.is_empty() || role.is_some_and(str::is_empty) {
            continue;
        }

        let mut participation = relation("user", "participated_in", event_phrase, 0.82);
        participation.status = status;
        out.push(participation);
        if let Some(role) = role {
            out.push(relation("user", "participant_role", role, 0.78));
        }
        out.push(relation(event_phrase, "event_type", event_head, 0.78));
        if let Some((relative, _)) = event_phrase
            .split_once("'s ")
            .or_else(|| event_phrase.split_once("’s "))
        {
            let relative = relative.trim();
            if !relative.is_empty() {
                out.push(relation(event_phrase, "event_of", relative, 0.76));
                out.push(relation(relative, "relative_of", "user", 0.72));
            }
        }
    }
    out
}

/// Extract first-person acquisitions using the same claim contract as event
/// relationships. The acquired object is kept as the claim object, with the
/// source evidence attached by the normal indexing pipeline.
fn extract_acquisition_relations(text: &str) -> Vec<Claim> {
    let mut claims = Vec::new();
    let rules = reasoning_rules();
    for sentence in text.split(['.', '!', '?', '\n']) {
        let lower = sentence.to_lowercase();
        let first_person = rules.acquisition_subject_markers.iter().any(|marker| {
            lower.contains(format!(" {marker} ").as_str())
                || lower
                    .trim_start()
                    .starts_with(format!("{marker} ").as_str())
        });
        if !first_person {
            continue;
        }
        let Some(frame) = rules
            .acquisition_frames
            .iter()
            .find(|frame| lower.contains(frame.as_str()))
        else {
            continue;
        };
        let frame_start = lower.find(frame.as_str()).unwrap_or_default();
        if !is_first_person_action_prefix(&lower[..frame_start]) {
            continue;
        }
        let start = frame_start + frame.len();
        let acquisition_tail = sentence[start..].trim_start();
        if frame == " got "
            && !rules
                .acquisition_got_object_prefixes
                .iter()
                .any(|prefix| acquisition_tail.to_lowercase().starts_with(prefix))
        {
            continue;
        }
        let tail = sentence[start..]
            .split([',', ';'])
            .next()
            .unwrap_or_default();
        let mut object_words = Vec::new();
        for word in tail.split_whitespace() {
            let normalized = word
                .trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase();
            if rules
                .object_stop_words
                .iter()
                .any(|word| word == &normalized)
            {
                break;
            }
            object_words.push(word);
        }
        let object = object_words
            .join(" ")
            .trim_start_matches("a ")
            .trim_start_matches("an ")
            .trim_start_matches("the ")
            .trim_start_matches("my ")
            .trim()
            .to_string();
        if object.is_empty() || is_relative_time_phrase(&object) {
            continue;
        }
        let mut claim = relation("user", "acquired", &object, 0.82);
        claim.status = Some(ClaimStatus::Completed);
        claims.push(claim);
    }
    claims
}

fn is_relative_time_phrase(value: &str) -> bool {
    let rules = reasoning_rules();
    let words: Vec<&str> = value.split_whitespace().collect();
    words.last() == Some(&"ago")
        && words.first().is_some_and(|first| {
            rules
                .relative_time_numbers
                .iter()
                .any(|number| first.trim_matches(|c: char| !c.is_alphanumeric()) == number)
                || first.chars().all(|c| c.is_ascii_digit())
        })
}

fn is_event_noun(noun: &str) -> bool {
    reasoning_rules()
        .event_nouns
        .iter()
        .any(|event| event == noun)
}

/// Accept an event verb only when it belongs to the first-person clause,
/// allowing a small set of auxiliaries and adverbs between "I" and the verb.
/// This rejects quoted/embedded reports such as "I heard my cousin attended"
/// and explicit negation such as "I didn't participate".
fn is_first_person_action_prefix(prefix: &str) -> bool {
    let allowed = &reasoning_rules().action_prefix_words;
    let clause_prefix = prefix
        .rfind(" i ")
        .map(|start| &prefix[start + 1..])
        .unwrap_or(prefix);
    let words: Vec<String> = clause_prefix
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'')
                .to_lowercase()
        })
        .filter(|word| !word.is_empty())
        .collect();
    !words.is_empty() && words[0] == "i" && words.iter().all(|word| allowed.contains(word))
}

fn relation(subject: &str, predicate: &str, object: &str, confidence: f32) -> Claim {
    Claim {
        subject: subject.to_string(),
        predicate: predicate.to_string(),
        object: object.to_string(),
        confidence,
        ..Default::default()
    }
}

fn is_valid_claim(claim: &Claim) -> bool {
    !claim.subject.trim().is_empty()
        && !claim.predicate.trim().is_empty()
        && !claim.object.trim().is_empty()
        && claim.confidence.is_finite()
}

fn dedupe_claims(claims: &mut Vec<Claim>) {
    claims.retain(is_valid_claim);
    claims.sort_by(|a, b| {
        a.subject
            .cmp(&b.subject)
            .then_with(|| a.predicate.cmp(&b.predicate))
            .then_with(|| a.object.cmp(&b.object))
            .then_with(|| a.status.cmp(&b.status))
            .then_with(|| a.event_time.cmp(&b.event_time))
            .then_with(|| {
                a.confidence
                    .partial_cmp(&b.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });
    claims.dedup_by(|a, b| {
        a.subject == b.subject
            && a.predicate == b.predicate
            && a.object == b.object
            && a.status == b.status
            && a.event_time == b.event_time
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Claim, Provenance, SectionChunk};
    use crate::tier1::{RankedTerm, Tier1Entity};

    fn sample_record() -> DocRecord {
        DocRecord {
            doc_id: "doc-1".to_string(),
            source: "source://doc-1".to_string(),
            content: "Alice works at Acme".into(),
            timestamp: Some("2024-05-01".to_string()),
            doc_length: 20,
            author_agent: None,
            group_id: Some("group-a".to_string()),
            filters: std::collections::BTreeMap::new(),
            probable_topic: Some("Alice".to_string()),
            doc_type_guess: Some("note".to_string()),
            headings: vec!["Overview".to_string()],
            doc_links: vec![],
            temporal_terms: vec![],
            key_entities: vec![Tier1Entity {
                text: "Alice".to_string(),
                label: "PROPN".to_string(),
                start: 0,
                end: 5,
                score: Some(1.0),
                source: "heuristic".to_string(),
            }],
            important_terms: vec![RankedTerm {
                term: "acme".to_string(),
                score: 1.0,
                source: "yake".to_string(),
            }],
            key_phrases: vec![],
            key_phrase_extraction_hash: String::new(),
            section_chunks: vec![SectionChunk {
                chunk_id: "doc-1::chunk-0".to_string(),
                heading: "Overview".to_string(),
                content: "Alice works at Acme".into(),
                start_line: 1,
                end_line: 1,
                timestamp: Some("2024-05-01".to_string()),
                key_entities: vec!["Alice".to_string()],
                important_terms: vec!["acme".to_string()],
            }],
            embedding: None,
            top_claims: vec![Claim {
                subject: "Alice".to_string(),
                predicate: "works_at".to_string(),
                object: "Acme".to_string(),
                confidence: 0.9,
                ..Default::default()
            }],
            provenance: Provenance {
                source: "source://doc-1".to_string(),
                timestamp: Some("2024-05-01".to_string()),
                ner_provider: "heuristic".to_string(),
                term_ranker: "yake".to_string(),
                index_version: "v1".to_string(),
            },
            content_hash: String::new(),
            semantic_tags: Vec::new(),
        }
    }

    #[test]
    fn conservative_extractor_uses_existing_claims_and_chunk_signals() {
        let extractor = ConservativeClaimExtractor;
        let record = sample_record();
        let extracted = extractor.extract(&record);
        assert!(extracted
            .claims
            .iter()
            .any(|c| c.predicate == "works_at" && c.object == "Acme"));
        assert!(extracted
            .claims
            .iter()
            .any(|c| c.predicate == "mentions_entity" && c.object == "Alice"));
        assert!(extracted
            .claims
            .iter()
            .any(|c| c.predicate == "important_term" && c.object == "acme"));
        assert!(extracted
            .claims
            .iter()
            .any(|c| c.predicate == "timestamp" && c.object == "2024-05-01"));
    }

    #[test]
    fn extracts_event_participant_role_and_family_edges_without_provider_signal() {
        let claims = extract_event_relations(
            "I recently walked down the aisle as a bridesmaid at my cousin's wedding.",
        );
        assert!(claims.iter().any(|claim| {
            claim.subject == "user"
                && claim.predicate == "participated_in"
                && claim.object == "cousin's wedding"
        }));
        assert!(claims.iter().any(|claim| {
            claim.subject == "user"
                && claim.predicate == "participant_role"
                && claim.object == "bridesmaid"
        }));
        assert!(claims.iter().any(|claim| {
            claim.subject == "cousin's wedding"
                && claim.predicate == "event_type"
                && claim.object == "wedding"
        }));
        assert!(claims.iter().any(|claim| {
            claim.subject == "cousin" && claim.predicate == "relative_of" && claim.object == "user"
        }));
    }

    #[test]
    fn extracts_direct_participation_frame_and_stops_at_event_noun() {
        let claims = extract_event_relations(
            "I attended my cousin's wedding last week, and we had a wonderful time.",
        );
        assert!(claims.iter().any(|claim| {
            claim.subject == "user"
                && claim.predicate == "participated_in"
                && claim.object == "cousin's wedding"
        }));
        assert!(claims.iter().any(|claim| {
            claim.subject == "cousin's wedding"
                && claim.predicate == "event_of"
                && claim.object == "cousin"
        }));
    }

    #[test]
    fn extracts_sports_events_from_distinct_first_person_action_frames() {
        let examples = [
            (
                "I'm looking for some new bike routes to try out. Do you have any suggestions for trails around the city? By the way, I just completed the Spring Sprint Triathlon today, which included a 20K bike ride, and I'm itching to get back on my bike.",
                "Spring Sprint Triathlon",
            ),
            (
                "I just finished a 5K run at the Midsummer 5K Run.",
                "Midsummer 5K Run",
            ),
            (
                "I participate in the company’s annual charity soccer tournament today.",
                "company’s annual charity soccer tournament",
            ),
        ];

        for (text, expected_event) in examples {
            let claims = extract_event_relations(text);
            assert!(
                claims.iter().any(|claim| {
                    claim.subject == "user"
                        && claim.predicate == "participated_in"
                        && claim.object == expected_event
                }),
                "missing participation claim for {text:?}: {claims:?}"
            );
            assert!(
                claims.iter().any(|claim| {
                    claim.subject == expected_event && claim.predicate == "event_type"
                }),
                "missing event type claim for {text:?}: {claims:?}"
            );
        }
    }

    #[test]
    fn records_future_participation_as_planned() {
        let claims = extract_event_relations(
            "I will participate in the company's annual charity soccer tournament today.",
        );
        let participation = claims
            .iter()
            .find(|claim| claim.predicate == "participated_in")
            .expect("planned participation should be extracted");
        assert_eq!(
            participation.object,
            "company's annual charity soccer tournament"
        );
        assert_eq!(participation.status, Some(ClaimStatus::Planned));
        assert!(claims.iter().any(|claim| {
            claim.subject == participation.object
                && claim.predicate == "event_type"
                && claim.object == "tournament"
        }));
    }

    #[test]
    fn extracts_completed_first_person_acquisitions() {
        let claims = extract_acquisition_relations(
            "I'm looking for BBQ sauce recipes. By the way, I just got a smoker today and I'm excited to use it.",
        );
        let acquired = claims
            .iter()
            .find(|claim| claim.predicate == "acquired")
            .expect("first-person acquisition should be extracted");
        assert_eq!(acquired.subject, "user");
        assert_eq!(acquired.object, "smoker");
        assert_eq!(acquired.status, Some(ClaimStatus::Completed));
    }

    #[test]
    fn does_not_treat_returning_from_an_event_as_an_acquisition() {
        let claims = extract_acquisition_relations(
            "I got back from a comic-con event and snagged a few rare comics.",
        );
        assert!(
            claims.is_empty(),
            "unexpected acquisition claims: {claims:?}"
        );
    }

    #[test]
    fn does_not_treat_an_acquisition_time_as_the_acquired_object() {
        let claims = extract_acquisition_relations(
            "I have a new pair of Nike Air Force 1s that I just got a few weeks ago.",
        );
        assert!(
            claims.is_empty(),
            "unexpected time-valued acquisition: {claims:?}"
        );
    }

    #[test]
    fn does_not_attribute_negated_or_embedded_participation_to_user() {
        for text in [
            "I didn't participate in the city race.",
            "I heard my cousin participated in a city race.",
        ] {
            let claims = extract_event_relations(text);
            assert!(
                !claims
                    .iter()
                    .any(|claim| claim.predicate == "participated_in"),
                "unexpected user participation for {text:?}: {claims:?}"
            );
        }
    }
}
