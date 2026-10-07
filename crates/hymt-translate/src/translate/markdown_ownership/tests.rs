use super::*;

#[test]
fn overlapping_owners_and_single_opaque_containment_match_naive_oracle() {
    let text = "abcdefghijklmnop";
    let leaves = vec![1..3, 3..8, 9..11, 12..14];
    for owners in [
        vec![0..5, 4..12],
        vec![0..16, 2..6, 2..8],
        vec![0..5, 5..12],
        vec![0..0, 8..8, 0..16],
    ] {
        let mut expected = Vec::new();
        for node in &owners {
            let mut cursor = node.start;
            for leaf in &leaves {
                if leaf.start < node.start || leaf.end > node.end {
                    continue;
                }
                if cursor < leaf.start {
                    expected.push(cursor..leaf.start);
                }
                cursor = leaf.end;
            }
            if cursor < node.end {
                expected.push(cursor..node.end);
            }
        }
        assert_eq!(
            merge_ranges(owned_gaps(text, owners, &leaves)),
            merge_ranges(expected)
        );
    }
    for opaque in [
        vec![0..5, 4..9],
        vec![0..16, 2..8],
        vec![0..0, 8..8],
        vec![0..3, 3..8],
    ] {
        let expected: Vec<_> = leaves
            .iter()
            .filter(|leaf| {
                !opaque
                    .iter()
                    .any(|node| node.start <= leaf.start && leaf.end <= node.end)
            })
            .cloned()
            .collect();
        let mut actual = leaves.clone();
        exclude_opaque(&mut actual, opaque);
        assert_eq!(
            actual, expected,
            "opaque union is not single-interval containment"
        );
    }
}

#[test]
fn structural_union_clips_utf8_seams_and_touching_ranges_once() {
    let text = "αβ世界\r\n尾";
    let ranges = merge_ranges(vec![0..0, 0..2, 2..7, 4..10, 10..10, 12..15, 12..15]);
    let sections = [0..4, 4..10, 10..12, 12..15];
    let mut cursor = 0;
    let mut bytes = String::new();
    for section in sections {
        let expected: Vec<_> = ranges
            .iter()
            .filter_map(|range| {
                let left = range.start.max(section.start);
                let right = range.end.min(section.end);
                (left < right).then(|| left - section.start..right - section.start)
            })
            .collect();
        let actual = clipped_ranges(&ranges, &mut cursor, section.clone());
        assert_eq!(actual, expected);
        for range in actual {
            assert!(text.is_char_boundary(section.start + range.start));
            assert!(text.is_char_boundary(section.start + range.end));
            bytes.push_str(&text[section.start + range.start..section.start + range.end]);
        }
    }
    assert_eq!(bytes, "αβ世界尾");
}

#[test]
fn fence_lookup_matches_original_suffix_scan() {
    let choices = [
        "plain\r\n",
        "```rust\r\n",
        "```\r\n",
        "````\n",
        "`````\n",
        " | a | b |\n",
        " |---|---|\n",
    ];
    // Bounded exhaustive width/info/table controls, not a timing benchmark.
    for case in 0..choices.len().pow(4) {
        let mut digits = case;
        let mut text = String::new();
        for _ in 0..4 {
            text.push_str(choices[digits % choices.len()]);
            digits /= choices.len();
        }
        let mut offset = 0;
        let lines: Vec<_> = text
            .split_inclusive('\n')
            .map(|line| {
                let start = offset;
                offset += line.len();
                (start, offset, line.trim_end_matches(['\r', '\n']))
            })
            .collect();
        let mut expected = Vec::new();
        let mut index = 0;
        while index < lines.len() {
            if let Some(width) = opening_fence_width(lines[index].2.trim_start()) {
                if let Some(relative) = lines[index + 1..]
                    .iter()
                    .position(|(_, _, line)| is_closing_fence(line, width))
                {
                    let closing = index + relative + 1;
                    expected.push(lines[index].0..lines[closing].1);
                    index = closing + 1;
                    continue;
                }
            }
            if index + 2 < lines.len()
                && is_markdown_table_line(lines[index].2)
                && is_markdown_table_separator(lines[index + 1].2)
                && is_markdown_table_line(lines[index + 2].2)
            {
                let mut end = index + 3;
                while end < lines.len() && is_markdown_table_line(lines[end].2) {
                    end += 1;
                }
                expected.push(lines[index].0..lines[end - 1].1);
                index = end;
                continue;
            }
            index += 1;
        }
        assert_eq!(
            protected_markdown_block_ranges(&text),
            expected,
            "case={text:?}"
        );
    }
}

#[test]
fn copy_only_reference_sections_keep_original_bytes() {
    let source = "[the public reference guide][]\r\n\r\n[the public reference guide]: ../guide#install \"Guide title\"\r\n";
    let mut plan = hymt_core::language::plan_document_translation(
        source,
        "zh",
        hymt_core::language::DocumentTranslationPolicy::TranslateAll,
    );
    for section in &mut plan.sections {
        section.should_translate = false;
    }
    make_short_references_explicit(&mut plan);
    let output: String = plan
        .sections
        .iter()
        .map(|section| section.text.as_str())
        .collect();
    assert_eq!(output, source);
}
