use super::*;

#[test]
fn paragraph_boundaries_accept_lf_crlf_and_blank_line_whitespace() {
    for text in [
        "first\n\nsecond\n\nthird",
        "first\r\n\r\nsecond\r\n\r\nthird",
        "\r\n first\r\n \t\nsecond\n\r\nthird\r\n\r\n",
        "first\n \t\nsecond\n\t\nthird",
    ] {
        assert_eq!(count_paragraphs(text), 3, "{text:?}");
    }
    for text in ["", "\n\r\n \t\n", "\t\r\n"] {
        assert_eq!(count_paragraphs(text), 0, "{text:?}");
    }
    // A lone CR or Unicode separator is content, not an LF/CRLF line ending.
    for text in [
        "first\r\rsecond",
        "first\u{2028}\u{2028}second",
        "first\nsecond",
    ] {
        assert_eq!(count_paragraphs(text), 1, "{text:?}");
    }
}

#[test]
fn paragraph_completeness_is_independent_of_newline_direction() {
    for source_sep in ["\n\n", "\r\n\r\n", "\r\n \t\n"] {
        let source = [
            "First complete instruction.",
            "Second complete instruction.",
            "Third complete instruction.",
        ]
        .join(source_sep);
        for output_sep in ["\n\n", "\r\n\r\n", "\n \t\r\n"] {
            let complete = [
                "请完整保留第一条操作说明。",
                "请完整保留第二条操作说明。",
                "请完整保留第三条操作说明。",
            ]
            .join(output_sep);
            for json in [false, true] {
                let wrap = |text: &str| {
                    if json {
                        serde_json::json!({"message": text}).to_string()
                    } else {
                        text.to_owned()
                    }
                };
                let valid = validate_completeness(&wrap(&source), &wrap(&complete), "zh", None);
                assert!(
                    valid.is_complete,
                    "{source_sep:?} -> {output_sep:?}, json={json}: {valid:?}"
                );
                let missing = validate_completeness(
                    &wrap(&source),
                    &wrap("请完整保留第一条操作说明，并保持所有样本完整可用。"),
                    "zh",
                    None,
                );
                assert!(
                    missing
                        .checks_failed
                        .iter()
                        .any(|code| code == "paragraph_count"),
                    "{source_sep:?}, json={json}: {missing:?}"
                );
            }
        }
    }
}
