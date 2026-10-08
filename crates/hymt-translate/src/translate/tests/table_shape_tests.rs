use super::*;

#[test]
fn table_shape_matches_parser_pipe_grammar() {
    use pulldown_cmark::{Options, Parser};

    for slashes in 1..=4 {
        let slash = "\\".repeat(slashes);
        for cells in [
            format!("first{slash}|literal | last"),
            format!("first | last{slash}|"),
            format!("{slash}|first | last"),
            format!("`first{slash}|literal` | last"),
            format!("甲{slash}|乙 | 丙"),
        ] {
            let plain = format!("A | B\n--- | ---\n{cells}\n");
            let edged = format!("| A | B |\r\n|---|---|\r\n  | {cells} | \t\r\n");
            // Independent parser oracle: optional edges leave every cell event unchanged.
            assert_eq!(
                Parser::new_ext(&plain, Options::ENABLE_TABLES).collect::<Vec<_>>(),
                Parser::new_ext(&edged, Options::ENABLE_TABLES).collect::<Vec<_>>()
            );
            for (source, output) in [(&plain, &edged), (&edged, &plain)] {
                assert_eq!(markdown_structure(source).tables, vec![(2, vec![2])]);
                assert!(ensure_markdown_structure_preserved(source, output).is_ok());
            }
        }
    }
    // Parser events pad/drop cells: raw-width admission must still distinguish them.
    for (row, width) in [("first", 1), ("`first|literal` | last", 3)] {
        let source = format!("A | B\n--- | ---\n{row}\n");
        let changed = "A | B\n--- | ---\nfirst | last\n";
        assert_eq!(markdown_structure(&source).tables, vec![(2, vec![width])]);
        assert!(ensure_markdown_structure_preserved(&source, changed).is_err());
    }
}

#[test]
fn table_shape_rejects_lost_rows_cells_and_changed_columns() {
    let source = "| Name | Meaning |\n|---|---|\n| Alpha | First |\n| Beta | Second |\n";
    for invalid in [
        "| 名称 | 意义 |\n|---|---|\n| 甲 | 第一 |\n",
        "| 名称 | 意义 |\n|---|---|\n| 甲 |\n| 乙 | 第二 |\n",
        "| 名称 | 意义 |\n|---|---|\n| 甲 | 第一 | 多余 |\n| 乙 | 第二 |\n",
        "| 名称 | 意义 | 新列 |\n|---|---|---|\n| 甲 | 第一 | 值 |\n| 乙 | 第二 | 值 |\n",
        // Same aggregate cell count, different per-row shape.
        "| 名称 | 意义 |\n|---|---|\n| 甲 |\n| 乙 | 第二 | 多余 |\n",
    ] {
        assert!(
            ensure_markdown_structure_preserved(source, invalid).is_err(),
            "accepted {invalid:?}"
        );
    }
}

#[test]
fn table_shape_preserves_translation_and_original_ragged_widths() {
    for (source, translated) in [
        (
            "| Name | Meaning |\n|---|---|\n| Alpha | First |\n",
            "名称 | 意义\r\n :--- | ---: \r\n甲 | 第一\r\n",
        ),
        (
            "| Name | Meaning |\n|---|---|\n| Alpha |\n| Beta | Second | Extra |\n",
            "| 名称 | 意义 |\n|---|---|\n| 甲 |\n| 乙 | 第二 | 额外 |\n",
        ),
        (
            "| Name | Meaning |\n|---|---|\n| a\\|b | `x\\|y` &#124; <b>bold</b> |\n",
            "| 名称 | 意义 |\n|---|---|\n| 甲\\|乙 | `x\\|y` &#124; <b>粗体</b> |\n",
        ),
    ] {
        assert!(ensure_markdown_structure_preserved(source, translated).is_ok());
    }
}

#[test]
fn table_shape_uses_parser_recognized_blocks_and_order() {
    let table = "| Name | Meaning |\n|---|---|\n| Alpha | First |\n";
    for wrap in [
        format!("```\n{table}```\n"),
        format!("<pre>\n{table}</pre>\n"),
    ] {
        let changed = wrap.replace("| Alpha | First |", "| Changed |");
        // Fenced code contents are not a table signature (the existing code contract is separate).
        assert_eq!(
            markdown_structure(&wrap).tables,
            markdown_structure(&changed).tables
        );
        assert!(markdown_structure(&wrap).tables.is_empty());
    }
    for prefix in ["> ", "  "] {
        let nested = table
            .lines()
            .map(|line| format!("{prefix}{line}\n"))
            .collect::<String>();
        assert_eq!(
            markdown_structure(&nested).tables,
            markdown_structure(table).tables
        );
        assert!(ensure_markdown_structure_preserved(
            &nested,
            &nested.replace("| Alpha | First |", "| 甲 |")
        )
        .is_err());
    }
    for source in [
        "| A | B |\n|---|---|\n| x\\|y | z |\n",
        "| A | B |\n|---|---|\n| `x\\|y` | &#124; <b>z</b> |\n",
        "A | B\n--- | ---\nx\\|y | z\\|\n",
        "| A | B |\n|---|---|\n| x\\\\ | y |\n",
        "- | A | B |\n  |---|---|\n  | x | y |\n",
    ] {
        assert_eq!(markdown_structure(source).tables, vec![(2, vec![2])]);
    }
    // GFM splits an unescaped pipe even in a code span, then trims excess cells.
    assert_eq!(
        markdown_structure("| A | B |\n|---|---|\n| `x|y` | z |\n").tables,
        vec![(2, vec![3])]
    );
    let small = "| A |\n|---|\n| B |\n";
    assert!(ensure_markdown_structure_preserved(
        &format!("{table}\n{small}"),
        &format!("{small}\n{table}")
    )
    .is_err());
}
