//! The driver's copy of the generated tables (`src/tables`): each module
//! as derivation/generated holds it, less the items the driver never
//! reads, and with those only its tests read under `#[cfg(test)]`. The
//! full record stays in derivation/generated; nothing here changes a value.

/// Modules the driver does not carry at all: step 4f's staged table,
/// which step 4f2's `band_paths` replaced (findings step 4f2).
pub const LEFT_OUT: [&str; 1] = ["band.rs"];

/// Per module, the items left out of the driver's copy: the record's own
/// readings and summaries, read by no driver code.
pub const OMIT: [(&str, &[&str]); 5] = [
    (
        "band_paths.rs",
        &["BYPASS", "RUNS", "TIE_DB", "DOMINANT", "BAND_PATH_GRID"],
    ),
    (
        "pll.rs",
        &[
            "LO_REACH",
            "LO_UNREACHABLE_HZ",
            "R2_VCO_INDICATOR",
            "SWEEP_RUNS",
            "VCO_RECEIVED_HZ",
            "VCO_RECEIVED_NINT",
        ],
    ),
    (
        "gain.rs",
        &["COMPARATOR_RESOLUTION_DB", "LOOP_METER_COUNTS_PER_DB"],
    ),
    (
        "if_filter.rs",
        &[
            "HPF_EFFECT",
            "HPF_HAS_NO_MEASURABLE_EFFECT",
            "REFERENCE_TWO_STATE_DB",
        ],
    ),
    (
        "types.rs",
        &[
            "BandEntry",
            "BandPoint",
            "BandPathPoint",
            "BandPath",
            "R2Row",
            "LoReach",
            "SweepRun",
            "HpfEffect",
        ],
    ),
];

/// Per module, the items only the driver's tests read.
pub const TEST_ONLY: [(&str, &[&str]); 1] = [("band_paths.rs", &["IF_HZ"])];

/// Modules only the driver's tests read: carried under `#[cfg(test)]`
/// (`tuner_image`: the recorded image `image::Front::image` is held to).
pub const TEST_ONLY_MODULES: [&str; 1] = ["tuner_image.rs"];

const ITEM_KINDS: [&str; 5] = [
    "pub const ",
    "pub static ",
    "pub struct ",
    "pub enum ",
    "pub type ",
];

fn of<'a>(table: &'a [(&str, &[&'a str])], file: &str) -> &'a [&'a str] {
    table
        .iter()
        .find(|(f, _)| *f == file)
        .map_or(&[], |(_, items)| items)
}

/// The item a line declares, if it begins one.
fn item_name(line: &str) -> Option<&str> {
    let rest = ITEM_KINDS.iter().find_map(|k| line.strip_prefix(k))?;
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

/// The last line of the item beginning at `start`: the line itself when it
/// ends with `;`, else the first closing line at the left margin after it.
fn item_end(lines: &[&str], start: usize) -> usize {
    if lines[start].trim_end().ends_with(';') {
        return start;
    }
    (start + 1..lines.len())
        .find(|&j| matches!(lines[j], "];" | "}" | "};"))
        .unwrap_or(lines.len() - 1)
}

/// The driver's copy of one generated module's text.
pub fn subset(file: &str, text: &str) -> String {
    let (omit, test_only) = (of(&OMIT, file), of(&TEST_ONLY, file));
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<&str> = Vec::new();
    // Doc comments and attributes wait for the item they belong to.
    let mut pending: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.starts_with("///") || line.starts_with("#[") {
            pending.push(line);
            i += 1;
            continue;
        }
        if let Some(name) = item_name(line) {
            let end = item_end(&lines, i);
            if omit.contains(&name) {
                pending.clear();
            } else {
                out.append(&mut pending);
                if test_only.contains(&name) {
                    out.push("#[cfg(test)]");
                }
                out.extend_from_slice(&lines[i..=end]);
            }
            i = end + 1;
            continue;
        }
        out.append(&mut pending);
        out.push(line);
        i += 1;
    }
    // The header's second line says what this copy is; runs of blank
    // lines a left-out item leaves become one.
    let mut text = String::new();
    let mut blank = false;
    for (n, line) in out.iter().enumerate() {
        if line.is_empty() && blank {
            continue;
        }
        blank = line.is_empty();
        text.push_str(line);
        text.push('\n');
        if n == 0 && !(omit.is_empty() && test_only.is_empty()) {
            text.push_str("// The driver's copy (examples/tables/driver.rs): the full record is derivation/generated/");
            text.push_str(file);
            text.push('\n');
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_are_left_out_kept_or_kept_for_tests() {
        let text = "// GENERATED\n//! Doc.\n\n/// A.\npub const BYPASS: u8 = 1;\n/// B.\npub const IF_HZ: f64 = 4.0;\n\n/// C.\npub const BAND_PATH_GRID: [u8; 2] = [\n    1,\n    2,\n];\n\n/// D.\npub const BAND_PATH_SEGMENTS: [u8; 1] = [\n    3,\n];\n";
        let s = subset("band_paths.rs", text);
        assert!(
            !s.contains("BYPASS")
                && !s.contains("BAND_PATH_GRID")
                && !s.contains("/// A.")
                && !s.contains("/// C."),
            "{s}"
        );
        assert!(s.contains("/// B.\n#[cfg(test)]\npub const IF_HZ"), "{s}");
        assert!(
            s.contains("/// D.\npub const BAND_PATH_SEGMENTS: [u8; 1] = [\n    3,\n];"),
            "{s}"
        );
        assert!(!s.contains("\n\n\n"), "{s}");
        assert!(s.starts_with("// GENERATED\n// The driver's copy"), "{s}");
    }

    #[test]
    fn a_module_with_nothing_left_out_is_copied_as_it_is() {
        let text = "// GENERATED\n/// A.\npub const X: u8 = 1;\n";
        assert_eq!(subset("other.rs", text), text);
    }
}
