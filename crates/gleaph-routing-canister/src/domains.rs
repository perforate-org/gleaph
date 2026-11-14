//! Domain-constrained slot completion (ADR 0117).
//!
//! The general router extracts slots with fixed semantic rules. A parameter may additionally
//! declare the canonical values it can execute plus alias surfaces, and this module completes a
//! missing slot by matching the question against that domain. Matching is script-agnostic by
//! construction: it compares characters case-insensitively, accepts a canonical value, an alias,
//! or a prefix of either (so `ホラー` reaches `ホラー映画`, `SF` reaches `SF映画`, and `horror`
//! reaches `ホラー映画`), refuses an ambiguous winner, and always emits the canonical graph value
//! rather than the surface the visitor typed.

use gleaph_routing_wire::{GeneralDomainValue, GeneralParameter, GeneralSlot, GeneralSlotLiteral};

/// Shortest prefix match, in characters. One-character prefixes would match almost anything.
const MIN_PREFIX_CHARS: usize = 2;

/// One completed parameter value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Completion {
    pub parameter: String,
    pub canonical: String,
    pub surface: String,
    /// Character span in the question text.
    pub start: usize,
    pub end: usize,
}

/// Complete or canonicalize every domain-backed parameter.
///
/// Returns the completions it applied, in parameter order. A parameter that already carries a slot
/// is canonicalized in place (its surface and span are kept); a missing one is filled from the text.
/// A domain-backed slot that cannot canonicalize is discarded, so the caller's required-slot and
/// abstain recomputation sees it as missing rather than executing the unvalidated surface.
pub(crate) fn complete(
    text: &str,
    parameters: &[GeneralParameter],
    slots: &mut Vec<GeneralSlot>,
) -> Vec<Completion> {
    let chars: Vec<char> = text.chars().collect();
    let mut applied = Vec::new();
    for parameter in parameters {
        if parameter.domain.is_empty() || parameter.parameter_type != "string" {
            continue;
        }
        let existing = slots.iter().position(|slot| slot.name == parameter.name);
        match existing {
            Some(index) => {
                let existing_value = match &slots[index].value {
                    GeneralSlotLiteral::Text(value) => value.clone(),
                    // A domain is a string contract. Any other literal is invalid for this
                    // parameter and must not survive completion.
                    _ => {
                        slots.remove(index);
                        continue;
                    }
                };
                let Some(canonical) = canonicalize(&existing_value, &parameter.domain) else {
                    slots.remove(index);
                    continue;
                };
                slots[index].value = GeneralSlotLiteral::Text(canonical.clone());
                applied.push(Completion {
                    parameter: parameter.name.clone(),
                    canonical,
                    surface: slots[index].surface.clone(),
                    start: slots[index].start as usize,
                    end: slots[index].end as usize,
                });
            }
            None => {
                let Some(found) = best_match(&chars, &parameter.domain) else {
                    continue;
                };
                let surface: String = chars[found.start..found.end].iter().collect();
                slots.push(GeneralSlot {
                    name: parameter.name.clone(),
                    value: GeneralSlotLiteral::Text(found.canonical.clone()),
                    surface: surface.clone(),
                    start: found.start as u32,
                    end: found.end as u32,
                    rule: "domain".to_owned(),
                });
                applied.push(Completion {
                    parameter: parameter.name.clone(),
                    canonical: found.canonical,
                    surface,
                    start: found.start,
                    end: found.end,
                });
            }
        }
    }
    applied
}

/// Whether any parameter in the card declares a domain.
pub(crate) fn has_domain(parameters: &[GeneralParameter]) -> bool {
    parameters
        .iter()
        .any(|parameter| !parameter.domain.is_empty())
}

struct Found {
    canonical: String,
    start: usize,
    end: usize,
    /// Length of the matched surface in characters; longer wins.
    matched: usize,
}

/// Iterate the surfaces that select one canonical value: the graph value first, then aliases.
fn surfaces(entry: &GeneralDomainValue) -> impl Iterator<Item = &str> {
    std::iter::once(entry.value.as_str()).chain(entry.aliases.iter().map(String::as_str))
}

/// The longest exact-or-prefix match over a domain, or `None` on no match or an ambiguous best.
fn best_match(chars: &[char], domain: &[GeneralDomainValue]) -> Option<Found> {
    let mut best: Option<Found> = None;
    let mut ambiguous = false;
    for entry in domain {
        for surface in surfaces(entry) {
            let value: Vec<char> = surface.chars().collect();
            if value.is_empty() {
                continue;
            }
            let Some((start, matched)) = find_longest(chars, &value) else {
                continue;
            };
            let end = start + matched;
            let candidate = Found {
                canonical: entry.value.clone(),
                start,
                end,
                matched,
            };
            match &best {
                None => best = Some(candidate),
                Some(current) => {
                    if candidate.matched > current.matched
                        || (candidate.matched == current.matched && candidate.start < current.start)
                    {
                        best = Some(candidate);
                        ambiguous = false;
                    } else if candidate.matched == current.matched
                        && candidate.start == current.start
                        && candidate.canonical != current.canonical
                    {
                        ambiguous = true;
                    }
                }
            }
        }
    }
    best.filter(|_| !ambiguous)
}

/// Canonicalize an extracted value against a domain, accepting a prefix of exactly one value or
/// alias. The returned value is always the graph value, never the alias surface.
fn canonicalize(value: &str, domain: &[GeneralDomainValue]) -> Option<String> {
    let value: Vec<char> = value.trim().chars().collect();
    if value.is_empty() {
        return None;
    }
    let mut exact = std::collections::BTreeSet::new();
    for entry in domain {
        for surface in surfaces(entry) {
            let candidate: Vec<char> = surface.chars().collect();
            if candidate.len() == value.len()
                && candidate
                    .iter()
                    .zip(value.iter())
                    .all(|(left, right)| chars_equal(*left, *right))
            {
                exact.insert(entry.value.as_str());
            }
        }
    }
    match exact.len() {
        1 => return exact.into_iter().next().map(str::to_owned),
        0 => {}
        _ => return None,
    }
    let mut prefixed = std::collections::BTreeSet::new();
    for entry in domain {
        for surface in surfaces(entry) {
            let candidate: Vec<char> = surface.chars().collect();
            if candidate.len() >= value.len()
                && candidate
                    .iter()
                    .zip(value.iter())
                    .all(|(left, right)| chars_equal(*left, *right))
            {
                prefixed.insert(entry.value.as_str());
            }
        }
    }
    match prefixed.len() {
        1 => prefixed.into_iter().next().map(str::to_owned),
        _ => None,
    }
}

/// Longest prefix of `value` found in `text`, returning `(start, length)` in characters.
///
/// The full value is tried first, then successively shorter prefixes down to `MIN_PREFIX_CHARS`.
/// An ASCII match must sit on word boundaries so `action` does not match inside `actionable`.
fn find_longest(text: &[char], value: &[char]) -> Option<(usize, usize)> {
    let max = value.len().min(text.len());
    for length in (MIN_PREFIX_CHARS..=max).rev() {
        let needle = &value[..length];
        if let Some(start) = find_window(text, needle) {
            return Some((start, length));
        }
    }
    None
}

fn find_window(text: &[char], needle: &[char]) -> Option<usize> {
    if needle.len() > text.len() {
        return None;
    }
    (0..=text.len() - needle.len()).find(|&start| {
        let window = &text[start..start + needle.len()];
        window
            .iter()
            .zip(needle.iter())
            .all(|(left, right)| chars_equal(*left, *right))
            && boundary_ok(text, start, needle.len(), needle)
    })
}

/// ASCII tokens need a boundary; ideographic and kana text has no word separator to enforce one.
fn boundary_ok(text: &[char], start: usize, length: usize, needle: &[char]) -> bool {
    let ascii_edge = needle.first().is_some_and(|c| c.is_ascii_alphanumeric())
        || needle.last().is_some_and(|c| c.is_ascii_alphanumeric());
    if !ascii_edge {
        return true;
    }
    let before_ok = start == 0 || !text[start - 1].is_ascii_alphanumeric();
    let end = start + length;
    let after_ok = end >= text.len() || !text[end].is_ascii_alphanumeric();
    before_ok && after_ok
}

/// Case-insensitive character equality without Unicode case folding tables (no dependency).
fn chars_equal(left: char, right: char) -> bool {
    if left == right {
        return true;
    }
    let mut left_lower = left.to_lowercase();
    let mut right_lower = right.to_lowercase();
    match (left_lower.next(), right_lower.next()) {
        (Some(a), Some(b)) => a == b && left_lower.next().is_none() && right_lower.next().is_none(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_parameter(name: &str, domain: &[&str]) -> GeneralParameter {
        GeneralParameter {
            name: name.to_owned(),
            parameter_type: "string".to_owned(),
            required: true,
            domain: domain
                .iter()
                .map(|value| GeneralDomainValue {
                    value: (*value).to_owned(),
                    aliases: Vec::new(),
                })
                .collect(),
        }
    }

    fn aliased_parameter(name: &str, values: &[(&str, &[&str])]) -> GeneralParameter {
        GeneralParameter {
            name: name.to_owned(),
            parameter_type: "string".to_owned(),
            required: true,
            domain: values
                .iter()
                .map(|(value, aliases)| GeneralDomainValue {
                    value: (*value).to_owned(),
                    aliases: aliases.iter().map(|alias| (*alias).to_owned()).collect(),
                })
                .collect(),
        }
    }

    fn slot(name: &str, value: &str, surface: &str, start: u32, end: u32) -> GeneralSlot {
        GeneralSlot {
            name: name.to_owned(),
            value: GeneralSlotLiteral::Text(value.to_owned()),
            surface: surface.to_owned(),
            start,
            end,
            rule: "category".to_owned(),
        }
    }

    #[test]
    fn fills_a_missing_slot_from_a_prefix_in_kana() {
        let parameters = [text_parameter(
            "genre",
            &["SF映画", "ホラー映画", "アクション映画"],
        )];
        let mut slots = Vec::new();
        let applied = complete("2020年代のホラー映画を教えて", &parameters, &mut slots);
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].canonical, "ホラー映画");
        assert_eq!(applied[0].surface, "ホラー映画");
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].name, "genre");
        assert_eq!(slots[0].start, 7);
        assert_eq!(slots[0].end, 12);
    }

    #[test]
    fn a_shorter_kana_surface_canonicalizes_to_the_declared_value() {
        let parameters = [text_parameter("genre", &["ホラー映画"])];
        let mut slots = Vec::new();
        let applied = complete("ホラーを見たい", &parameters, &mut slots);
        assert_eq!(applied.len(), 1);
        assert_eq!(
            slots[0].value,
            GeneralSlotLiteral::Text("ホラー映画".to_owned())
        );
        assert_eq!(applied[0].surface, "ホラー");
    }

    #[test]
    fn canonicalizes_an_existing_rule_slot_in_place() {
        let parameters = [text_parameter("genre", &["SF映画"])];
        let mut slots = vec![slot("genre", "SF", "SF", 11, 13)];
        let applied = complete("1990年代のSF映画", &parameters, &mut slots);
        assert_eq!(applied.len(), 1);
        assert_eq!(
            slots[0].value,
            GeneralSlotLiteral::Text("SF映画".to_owned())
        );
        assert_eq!(slots[0].start, 11);
        assert_eq!(slots[0].surface, "SF");
    }

    #[test]
    fn discards_an_existing_slot_that_cannot_canonicalize() {
        let parameters = [text_parameter("genre", &["SF映画", "ホラー映画"])];
        let mut slots = vec![slot("genre", "の", "の", 10, 11)];

        let applied = complete(
            "1990年代のジャンプ作品の映画を見せて",
            &parameters,
            &mut slots,
        );

        assert!(applied.is_empty());
        assert!(
            slots.is_empty(),
            "an uncanonicalized domain slot must not reach execution"
        );
    }

    #[test]
    fn discards_a_non_text_domain_slot() {
        let parameters = [text_parameter("genre", &["SF映画"])];
        let mut slots = vec![GeneralSlot {
            name: "genre".to_owned(),
            value: GeneralSlotLiteral::Int(7),
            surface: "7".to_owned(),
            start: 0,
            end: 1,
            rule: "category".to_owned(),
        }];

        assert!(complete("7", &parameters, &mut slots).is_empty());
        assert!(slots.is_empty());
    }

    #[test]
    fn ascii_matching_is_case_insensitive_and_boundary_checked() {
        let parameters = [text_parameter("genre", &["Horror Film"])];
        let mut slots = Vec::new();
        complete("a horror film please", &parameters, &mut slots);
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].start, 2);

        let mut slots = Vec::new();
        complete("a horrorfilmy thing", &parameters, &mut slots);
        assert!(slots.is_empty(), "prefix must sit on a word boundary");
    }

    #[test]
    fn refuses_an_ambiguous_prefix() {
        let parameters = [text_parameter("genre", &["SF映画", "SF小説"])];
        let mut slots = Vec::new();
        let applied = complete("SFが好き", &parameters, &mut slots);
        assert!(applied.is_empty());
        assert!(slots.is_empty());
    }

    #[test]
    fn prefers_the_longest_match() {
        let parameters = [text_parameter("genre", &["アクション", "アクション映画"])];
        let mut slots = Vec::new();
        complete("アクション映画を探す", &parameters, &mut slots);
        assert_eq!(
            slots[0].value,
            GeneralSlotLiteral::Text("アクション映画".to_owned())
        );
    }

    #[test]
    fn fills_and_canonicalizes_from_an_alias() {
        let parameters = [aliased_parameter(
            "genre",
            &[("ホラー映画", &["horror", "scary movie"])],
        )];
        let mut slots = Vec::new();
        complete("a horror film please", &parameters, &mut slots);
        assert_eq!(slots.len(), 1);
        assert_eq!(
            slots[0].value,
            GeneralSlotLiteral::Text("ホラー映画".to_owned())
        );
        assert_eq!(slots[0].surface, "horror");

        let mut slots = vec![slot("genre", "scary", "scary", 0, 5)];
        let applied = complete("scary movie night", &parameters, &mut slots);
        assert_eq!(applied.len(), 1);
        assert_eq!(
            slots[0].value,
            GeneralSlotLiteral::Text("ホラー映画".to_owned())
        );
        assert_eq!(slots[0].surface, "scary");
    }

    #[test]
    fn refuses_an_alias_shared_by_two_values() {
        let parameters = [aliased_parameter(
            "genre",
            &[("ホラー映画", &["scary"]), ("スリラー映画", &["scary"])],
        )];
        let mut slots = Vec::new();
        assert!(complete("scary", &parameters, &mut slots).is_empty());
        assert!(slots.is_empty());
    }

    #[test]
    fn parameters_without_a_domain_are_untouched() {
        let parameters = [text_parameter("query", &[])];
        let mut slots = Vec::new();
        assert!(complete("宇宙の映画", &parameters, &mut slots).is_empty());
        assert!(slots.is_empty());
    }
}
