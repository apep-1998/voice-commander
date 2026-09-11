//! Table merging and the unknown-key check, both of which work on raw TOML rather than on
//! the typed structs.
//!
//! Doing inheritance before deserialization is what keeps the typed configuration free of
//! `Option` everywhere: by the time `Profile` is built, `[defaults.capture]` and
//! `[profiles.x.capture]` have already become one complete table.

use toml::{Table, Value};

/// Merge `overlay` into `base`, recursing into tables.
///
/// Arrays replace rather than concatenate. That is the behaviour a user expects from
/// `sinks = ["a"]` in a drop-in file: it should mean "these sinks", not "these as well as
/// whatever was configured elsewhere", which would be impossible to undo.
pub(super) fn merge_into(base: &mut Table, overlay: Table) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(Value::Table(existing)), Value::Table(incoming)) => {
                merge_into(existing, incoming);
            }
            (_, incoming) => {
                base.insert(key, incoming);
            }
        }
    }
}

/// Every leaf path present in `table`, dotted, e.g. `profiles.dictate.capture.pre_roll_ms`.
///
/// Arrays are treated as leaves: their contents are values, not configuration keys.
fn leaf_paths(table: &Table, prefix: &str, out: &mut Vec<String>) {
    for (key, value) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match value {
            Value::Table(inner) if !inner.is_empty() => leaf_paths(inner, &path, out),
            _ => out.push(path),
        }
    }
}

/// Keys the user wrote that the configuration schema does not have a home for.
///
/// Rather than maintaining a list of valid keys by hand — which would rot the moment a field
/// is added — this compares the document the user wrote against the document the parsed
/// configuration serializes back to. Anything in the first that is missing from the second
/// was silently dropped, which is exactly the class of typo that leaves a user staring at a
/// setting that appears to do nothing.
///
/// `free_form` lists path prefixes whose children are arbitrary by design — HTTP headers,
/// environment variables, multipart fields — and must not be reported.
pub(super) fn unknown_keys(
    input: &Table,
    round_tripped: &Table,
    free_form: &[&str],
) -> Vec<String> {
    let mut written = Vec::new();
    leaf_paths(input, "", &mut written);
    let mut understood = Vec::new();
    leaf_paths(round_tripped, "", &mut understood);

    written
        .into_iter()
        .filter(|path| !is_understood(path, &understood))
        .filter(|path| !free_form.iter().any(|p| path.starts_with(p)))
        .collect()
}

/// Whether a key the user wrote survived the round trip.
///
/// An exact match is the common case. The prefix case covers a table written empty — a bare
/// `[profiles.dictate]` that takes everything by inheritance is a leaf in the input and a
/// whole subtree in the output, and is obviously not a typo. A table that is empty *and*
/// has no subtree in the output still gets reported, which is the case worth catching.
fn is_understood(written: &str, understood: &[String]) -> bool {
    let prefix = format!("{written}.");
    understood
        .iter()
        .any(|known| known == written || known.starts_with(&prefix))
}

/// The valid key at the same level whose name is closest to `unknown`, if one is close
/// enough to be worth suggesting.
pub(super) fn suggest(unknown: &str, round_tripped: &Table) -> Option<String> {
    let (parent, leaf) = match unknown.rsplit_once('.') {
        Some((parent, leaf)) => (parent, leaf),
        None => ("", unknown),
    };

    let mut candidates = Vec::new();
    leaf_paths(round_tripped, "", &mut candidates);

    candidates
        .into_iter()
        .filter_map(|path| {
            let (cand_parent, cand_leaf) = match path.rsplit_once('.') {
                Some((p, l)) => (p.to_owned(), l.to_owned()),
                None => (String::new(), path.clone()),
            };
            (cand_parent == parent).then_some((cand_leaf, path))
        })
        .map(|(cand_leaf, path)| (edit_distance(leaf, &cand_leaf), path))
        // Beyond a third of the word being wrong it stops being a typo and starts being a
        // different word, where a suggestion misleads more than it helps.
        .filter(|(distance, _)| *distance * 3 <= leaf.len().max(1))
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, path)| path)
}

/// Levenshtein distance, two rows at a time.
pub(super) fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0; b.len() + 1];

    for (i, ca) in a.chars().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitute = prev[j] + usize::from(ca != *cb);
            curr[j + 1] = substitute.min(prev[j + 1] + 1).min(curr[j] + 1);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> Table {
        text.parse().expect("valid toml")
    }

    #[test]
    fn merging_recurses_into_nested_tables() {
        let mut base = table("[a.b]\nx = 1\ny = 2\n");
        merge_into(&mut base, table("[a.b]\ny = 3\nz = 4\n"));

        let inner = base["a"]["b"].as_table().expect("table");
        assert_eq!(inner["x"].as_integer(), Some(1));
        assert_eq!(inner["y"].as_integer(), Some(3));
        assert_eq!(inner["z"].as_integer(), Some(4));
    }

    #[test]
    fn a_scalar_overwrites_a_table_of_the_same_name() {
        // Not a case any sane configuration hits, but the merge must be total.
        let mut base = table("[a]\nx = 1\n");
        merge_into(&mut base, table("a = 5\n"));
        assert_eq!(base["a"].as_integer(), Some(5));
    }

    #[test]
    fn arrays_are_replaced_wholesale() {
        let mut base = table("xs = [1, 2, 3]\n");
        merge_into(&mut base, table("xs = [9]\n"));
        assert_eq!(base["xs"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn unknown_keys_finds_only_what_was_dropped() {
        let written = table("[a]\nkept = 1\ndropped = 2\n");
        let round_tripped = table("[a]\nkept = 1\nadded_by_default = 3\n");

        let unknown = unknown_keys(&written, &round_tripped, &[]);
        assert_eq!(unknown, vec!["a.dropped".to_owned()]);
    }

    #[test]
    fn an_empty_table_is_not_reported_when_the_schema_fills_it_in() {
        // `[profiles.p]` taking everything by inheritance is a leaf in the input and a whole
        // subtree in the output.
        let written = table("[profiles.p]\n");
        let round_tripped = table("[profiles.p.capture]\nmode = \"warm\"\n");
        assert!(unknown_keys(&written, &round_tripped, &[]).is_empty());
    }

    #[test]
    fn an_empty_table_with_no_counterpart_is_reported() {
        let written = table("[profiles.p.captur]\n");
        let round_tripped = table("[profiles.p.capture]\nmode = \"warm\"\n");
        assert_eq!(
            unknown_keys(&written, &round_tripped, &[]),
            vec!["profiles.p.captur".to_owned()]
        );
    }

    #[test]
    fn free_form_prefixes_are_exempt_but_their_siblings_are_not() {
        let written = table("[s.headers]\nX-Anything = \"1\"\n[s]\ntimeout_msec = 5\n");
        let round_tripped = table("[s]\ntimeout_ms = 5\n[s.headers]\n");

        // The header name is the user's to choose; the misspelled sibling is not.
        assert_eq!(
            unknown_keys(&written, &round_tripped, &["s.headers."]),
            vec!["s.timeout_msec".to_owned()]
        );
    }

    #[test]
    fn suggestions_only_consider_keys_at_the_same_level() {
        let round_tripped = table("[a]\npre_roll_ms = 1\n[b]\npre_roll_ms = 1\n");
        assert_eq!(
            suggest("a.pre_roll_m", &round_tripped),
            Some("a.pre_roll_ms".to_owned())
        );
    }

    #[test]
    fn a_wildly_different_name_gets_no_suggestion() {
        // Suggesting `pre_roll_ms` for `banana` would be worse than saying nothing.
        let round_tripped = table("[a]\npre_roll_ms = 1\n");
        assert_eq!(suggest("a.banana", &round_tripped), None);
    }

    #[test]
    fn edit_distance_counts_single_edits() {
        assert_eq!(edit_distance("", ""), 0);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("abc", "abd"), 1);
        assert_eq!(edit_distance("abc", "ab"), 1);
        assert_eq!(edit_distance("ab", "ba"), 2);
        assert_eq!(edit_distance("", "abc"), 3);
    }
}
