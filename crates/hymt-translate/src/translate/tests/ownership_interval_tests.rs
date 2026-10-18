use super::*;
use std::cell::Cell;

thread_local! {
    static VISITS: Cell<[usize; 4]> = const { Cell::new([0; 4]) };
}

pub(in super::super) fn visit(kind: usize) {
    VISITS.with(|counts| {
        let mut next = counts.get();
        next[kind] += 1;
        counts.set(next);
    });
}

fn counted<T>(run: impl FnOnce() -> T) -> (T, [usize; 4]) {
    VISITS.with(|counts| counts.set([0; 4]));
    let result = run();
    (result, VISITS.with(Cell::get))
}

async fn translated_source(source: &str) -> (String, Vec<String>) {
    let segmenter = fallback_segmenter();
    let planning_config = leaf_fixture_config(&make_stream_config("http://127.0.0.1:9/v1"), source);
    let opts = PromptOpts {
        document_translation_policy: Some(DocumentTranslationPolicy::TranslateAll),
        ..PromptOpts::default()
    };
    let plan = plan_translation(
        source,
        "zh",
        &planning_config,
        &segmenter,
        &TemplateType::Default,
        &opts,
    )
    .unwrap();
    let server = start_capturing_mock_server(
        plan.segments
            .iter()
            .map(|segment| {
                MockResponse::Json(if segment == "[]" {
                    segment.clone()
                } else {
                    "译".repeat(segment.chars().count().div_ceil(2))
                })
            })
            .collect(),
    )
    .await;
    let config = leaf_fixture_config(&make_stream_config(&server.endpoint_url), source);
    let tmp = tempfile::tempdir().unwrap();
    let history = HistoryDB::new(tmp.path().join("history.db"));
    let client = TranslationClient::new(config.clone()).unwrap();
    let ctx = TranslationCtx {
        config: &config,
        client: &client,
        segmenter: &segmenter,
        history: &history,
        cache_enabled: false,
    };
    let outcome = translate_text(source, "zh", &TemplateType::Default, &opts, &ctx)
        .await
        .unwrap();
    assert!(
        outcome.completeness_degraded_segments.is_empty(),
        "fixture must not bypass completeness"
    );
    let requests = server
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| {
            let (_, body) = request.split_once("\r\n\r\n").expect("captured HTTP body");
            let body: serde_json::Value = serde_json::from_str(body).unwrap();
            let prompt = body["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap();
            prompt
                .split_once("\n\n")
                .expect("default prompt source boundary")
                .1
                .to_owned()
        })
        .collect();
    (outcome.text, requests)
}

async fn reference_label(form: &str) {
    let source =
        format!("{form}\n\n[the public reference guide]: ../guide#install \"Guide title\"\n");
    let (output, requests) = translated_source(&source).await;
    assert!(
        output.contains("[译"),
        "visible reference label was not translated: {output:?}"
    );
    assert!(
        requests
            .iter()
            .any(|request| request.contains("the public reference guide")),
        "label must reach the provider"
    );
    assert!(requests
        .iter()
        .all(|request| !request.contains("../guide#install") && !request.contains("Guide title")));
    assert_eq!(markdown_structure(&source), markdown_structure(&output));
    assert!(output.contains("[the public reference guide]: ../guide#install \"Guide title\""));
}

#[tokio::test]
async fn shortcut_link_label_reaches_provider() {
    reference_label("[the public reference guide]").await;
}
#[tokio::test]
async fn collapsed_link_label_reaches_provider() {
    reference_label("[the public reference guide][]").await;
}
#[tokio::test]
async fn shortcut_image_label_reaches_provider() {
    reference_label("![the public reference guide]").await;
}
#[tokio::test]
async fn collapsed_image_label_reaches_provider() {
    reference_label("![the public reference guide][]").await;
}

#[tokio::test]
async fn reference_binding_uses_parser_normalization_and_first_definition() {
    for source in [
        "[Mixed   ID] [mixed id][] ![MIXED ID] ![Mixed ID][]\n\n[Mixed ID]: ../first#片段 \"First title\"\n[mixed id]: /wrong \"Wrong title\"\n",
        "[Straße] ![STRASSE][]\n\n[strasse]: ../unicode#目标 \"Unicode title\"\n",
        "[the public reference guide \\]] ![the public reference guide \\[]\n\n[the public reference guide \\]]: ../close#目标 \"Close title\"\n[the public reference guide \\[]: ../open#目标 \"Open title\"\n",
        "![the \\*public\\* guide &amp; café][]\n\n[the \\*public\\* guide &amp; café]: ../atom#目标 \"Atom title\"\n",
        "[![the public reference guide]](../outer \"Outer title\")\n\n[the public reference guide]: ../image#目标 \"Image title\"\n",
        "[the public reference guide][MiXeD   ID] ![the public reference guide][mixed id]\n\n[Mixed ID]: ../first#片段 \"First title\"\n",
        "[the public reference guide](../guide#片段 \"Title\") ![the public reference guide](../image.png \"Image title\")\n",
    ] {
        let (output, requests) = translated_source(source).await;
        assert!(output.contains("[译"), "display text must translate: {output:?}");
        assert!(!requests.is_empty());
        assert!(Parser::new(&output).all(|event| !matches!(event, Event::Text(text) if text.chars().any(|character| character.is_ascii_alphabetic()))), "every display label must translate: {output:?}");
        let structure = markdown_structure(source);
        for (target, title) in structure.links.iter().chain(&structure.images) {
            assert!(requests.iter().all(|request| !request.contains(target) && (title.is_empty() || !request.contains(title))), "bound destination/title must remain source-owned");
        }
        assert_eq!(markdown_structure(source), markdown_structure(&output), "original resolved target/title binding: {output:?}");
    }
}

#[tokio::test]
async fn escaped_punctuation_is_source_owned_atomically() {
    let source = "[the \\*public\\* guide](../guide#install \"Guide title\")\n";
    let (output, requests) = translated_source(source).await;
    assert_eq!(
        output.matches("\\*").count(),
        2,
        "escaped punctuation must survive the actual translation: {output:?}"
    );
    assert!(requests
        .iter()
        .all(|request| !request.contains('*') && !request.contains("../guide#install")));
    assert!(output.contains("译"));
    assert_eq!(markdown_structure(source), markdown_structure(&output));
}

#[tokio::test]
async fn nested_inline_entity_html_and_code_controls() {
    let source = "# **the \\*public\\* guide &amp; café 世界**\r\n\r\n[<span title=\"keep\">the \\*public\\* guide &amp; café</span> `keep code`](../guide#install \"Guide title\")\r\n\r\n![the \\*public\\* guide &amp; café](../image.png \"Image title\")\r\n\r\n```rust\r\nkeep_fence();\r\n```\r\n";
    let (output, requests) = translated_source(source).await;
    assert_eq!(output.matches("\\*").count(), 6);
    assert_eq!(output.matches("&amp;").count(), 3);
    for protected in [
        "<span title=\"keep\">",
        "</span>",
        "`keep code`",
        "```rust\r\nkeep_fence();\r\n```",
    ] {
        assert!(output.contains(protected), "lost {protected:?}: {output:?}");
    }
    assert!(requests.iter().all(|request| !request.contains('*')
        && !request.contains("&amp;")
        && !request.contains("keep_fence")
        && !request.contains("<span")
        && !request.contains("keep code")));
    assert_eq!(markdown_structure(source), markdown_structure(&output));
}

#[test]
fn actual_owner_opaque_and_section_scan_visits_scale_with_input() {
    for n in [128, 256] {
        let source = "# Heading `code`\n\n".repeat(n);
        let config = make_stream_config("http://127.0.0.1:9/v1");
        let segmenter = fallback_segmenter();
        let (plan, visits) = counted(|| {
            plan_translation(
                &source,
                "zh",
                &config,
                &segmenter,
                &TemplateType::Default,
                &PromptOpts::default(),
            )
            .unwrap()
        });
        assert_eq!(plan.reconstruct(&plan.segments), source);
        for (kind, count) in visits[..3].iter().enumerate() {
            assert!(
                *count <= n * 16,
                "actual scan kind={kind} n={n} visits={visits:?}"
            );
        }
        eprintln!("actual ownership scans n={n} visits={visits:?}");
    }
}

#[test]
fn unmatched_fence_scan_visits_scale_without_changing_ranges() {
    for n in [128, 256] {
        let source = "```rust\nplain text\n".repeat(n);
        let (ranges, visits) = counted(|| protected_markdown_block_ranges(&source));
        assert!(
            ranges.is_empty(),
            "unmatched backtick openers remain unprotected by this helper"
        );
        assert!(
            visits[3] <= n * 16,
            "actual fence suffix visits n={n} visits={visits:?}"
        );
        eprintln!("actual fence scans n={n} visits={visits:?}");
    }
}

#[tokio::test]
async fn malformed_multiline_link_repair_is_rejected_atomically_in_all_publication_modes() {
    let source = "[broken\nlabel]](/unclosed";
    let destination = "/unclosed";
    let source_structure = markdown_structure(source);
    assert!(
        source_structure.links.is_empty(),
        "malformed CommonMark must remain literal"
    );
    assert!(
        has_unparsed_link_syntax(source),
        "CommonMark literal must be recognized before Optimistic publication"
    );
    assert!(!has_unparsed_link_syntax("[valid](https://example.test)"));
    assert!(!has_unparsed_link_syntax("`literal ]( code`"));

    let opts = PromptOpts {
        document_translation_policy: Some(DocumentTranslationPolicy::TranslateAll),
        ..PromptOpts::default()
    };
    let segmenter = fallback_segmenter();
    let planning_config = make_stream_config("http://127.0.0.1:9/v1");
    let plan = plan_translation(
        source,
        "zh",
        &planning_config,
        &segmenter,
        &TemplateType::Default,
        &opts,
    )
    .unwrap();
    let planned_inputs = plan.segments.clone();
    let boundary_segment = planned_inputs
        .iter()
        .position(|segment| segment.contains("]("))
        .expect("malformed link delimiter must reach a planned model segment");
    let mock_replies: Vec<_> = planned_inputs
        .iter()
        .enumerate()
        .map(|(index, segment)| {
            let translation = "译".repeat((segment.len() / 3).max(1));
            if index == boundary_segment {
                format!("{translation} [修复后的链接]({destination})")
            } else {
                translation
            }
        })
        .collect();
    let expected_output = plan.reconstruct(&mock_replies);
    assert_eq!(
        markdown_structure(&expected_output).links,
        vec![(destination.to_owned(), String::new())],
        "adversarial mock must turn the literal into a parsed link"
    );

    for output_mode in [
        None,
        Some(StreamOutputMode::Validated),
        Some(StreamOutputMode::Optimistic),
    ] {
        let streaming = output_mode.is_some();
        // Match the existing validated-stream fixture: only the prioritized first request streams.
        let responses = mock_replies
            .iter()
            .cloned()
            .enumerate()
            .flat_map(|(index, response)| {
                let initial = if streaming && index == 0 {
                    MockResponse::Sse(vec![response.clone()])
                } else {
                    MockResponse::Json(response.clone())
                };
                let retry = (index == boundary_segment).then_some(MockResponse::Json(response));
                [Some(initial), retry].into_iter().flatten()
            })
            .collect();
        let server = start_capturing_mock_server(responses).await;
        let config = make_stream_config(&server.endpoint_url);
        let runtime_plan = plan_translation(
            source,
            "zh",
            &config,
            &segmenter,
            &TemplateType::Default,
            &opts,
        )
        .unwrap();
        assert_eq!(runtime_plan.segments, planned_inputs);

        let tmp = tempfile::tempdir().unwrap();
        let history = HistoryDB::new(tmp.path().join("history.db"));
        let client = TranslationClient::new(config.clone()).unwrap();
        let ctx = TranslationCtx {
            config: &config,
            client: &client,
            segmenter: &segmenter,
            history: &history,
            cache_enabled: false,
        };

        if let Some(output_mode) = output_mode {
            let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(64);
            let error = translate_text_stream_with_mode(
                source,
                "zh",
                &TemplateType::Default,
                &opts,
                &ctx,
                output_mode,
                event_tx,
            )
            .await
            .expect_err("malformed literal repair must be rejected");
            assert!(format!("{error:#}").contains("translated Markdown structure changed"));
            let mut events = Vec::new();
            while let Some(event) = event_rx.recv().await {
                events.push(event);
            }
            assert!(
                events.is_empty(),
                "unvalidated stream output leaked: {events:?}"
            );
        } else {
            let input_path = tmp.path().join("malformed.md");
            let output_path = tmp.path().join("existing.md");
            std::fs::write(&input_path, source).unwrap();
            std::fs::write(&output_path, "existing translation\n").unwrap();
            let error = translate_file(
                &input_path,
                Some(&output_path),
                "zh",
                &TemplateType::Default,
                &opts,
                &ctx,
            )
            .await
            .expect_err("malformed literal repair must be rejected");
            assert!(format!("{error:#}").contains("translated Markdown structure changed"));
            assert_eq!(
                std::fs::read(&output_path).unwrap(),
                b"existing translation\n",
                "rejection must preserve the existing output file"
            );
        }

        let requests = server.requests.lock().unwrap().clone();
        assert_eq!(
            requests.len(),
            planned_inputs.len() + 1,
            "malformed boundary must use one structural retry"
        );
        let captured_inputs: Vec<_> = requests
            .iter()
            .enumerate()
            .map(|(index, request)| {
                let (_, body) = request.split_once("\r\n\r\n").expect("captured HTTP body");
                let payload: serde_json::Value = serde_json::from_str(body).unwrap();
                assert_eq!(
                    payload["stream"].as_bool() == Some(true),
                    streaming && index == 0
                );
                let prompt = payload["messages"]
                    .as_array()
                    .and_then(|messages| messages.last())
                    .and_then(|message| message["content"].as_str())
                    .unwrap();
                prompt
                    .split_once("\n\n")
                    .expect("default prompt source boundary")
                    .1
                    .to_owned()
            })
            .collect();
        assert_eq!(captured_inputs.len(), planned_inputs.len() + 1);
        for planned in &planned_inputs {
            assert!(
                captured_inputs
                    .iter()
                    .any(|captured| captured.contains(planned.trim())),
                "planned input was not captured: {planned:?}"
            );
        }
        assert!(
            captured_inputs
                .iter()
                .any(|input| input.contains("]](/unclosed")),
            "malformed multiline syntax must reach the mock provider"
        );
    }
}

#[test]
fn malformed_inline_link_softbreak_is_guarded_before_optimistic_publication() {
    let source =
        "Translate the malformed link [broken]\n(unclosed destination with enough surrounding words to explain the literal.";
    assert!(markdown_structure(source).links.is_empty());
    assert!(
        has_unparsed_link_syntax(source),
        "a parser soft break must not hide an unparsed inline-link candidate"
    );
}

#[test]
fn unresolved_reference_link_is_guarded_before_optimistic_publication() {
    let source =
        "Translate this unresolved reference [broken][missing] with enough surrounding words to explain the literal.";
    assert!(markdown_structure(source).links.is_empty());
    assert!(
        has_unparsed_link_syntax(source),
        "an unresolved parser reference must be recognized before Optimistic publication"
    );
}

#[test]
fn markdown_link_classification_preserves_valid_ownership_and_guards_unresolved_forms() {
    for source in [
        "[inline](https://example.test)",
        "[multi\nline](https://example.test)",
        "[label][ref]\n\n[ref]: /target",
        "[label][]\n\n[label]: /target",
        "[label]\n\n[label]: /target",
        "![image](https://example.test/image.png)",
        "![image][ref]\n\n[ref]: /image.png",
        "![image][]\n\n[image]: /image.png",
        "![image]\n\n[image]: /image.png",
    ] {
        let structure = markdown_structure(source);
        assert!(
            structure.links.len() + structure.images.len() > 0,
            "valid parser control was not a link: {source:?}"
        );
        assert!(
            !has_unparsed_link_syntax(source),
            "valid parser-owned syntax must stay Optimistic: {source:?}"
        );
    }
    for source in [
        "[broken][missing]",
        "[broken][]",
        "[broken]",
        "![broken][missing]",
        "![broken][]",
        "![broken]",
    ] {
        let structure = markdown_structure(source);
        assert!(
            structure.links.is_empty() && structure.images.is_empty(),
            "unresolved syntax must remain literal at the CommonMark layer: {source:?}"
        );
        assert!(
            has_unparsed_link_syntax(source),
            "unresolved parser reference must select Validated mode: {source:?}"
        );
    }
    assert!(has_unparsed_link_syntax("[broken]\n(unclosed destination)"));
    assert!(has_unparsed_link_syntax(
        r"\[escaped\]\(https://example.test\)"
    ));
    assert!(!has_unparsed_link_syntax("`literal ]( code`"));
    assert!(!has_unparsed_link_syntax("```\n[broken][missing]\n```"));
    assert!(!has_unparsed_link_syntax(
        "ordinary Optimistic text without Markdown links"
    ));
}

#[tokio::test]
async fn literal_emphasis_keeps_delimiters_and_flanking_punctuation() {
    for source in [
        "**Label:**Body continues here.",
        "**Label：**Body continues here.",
        "__Label:__Body continues here.",
        "**Label:**Body continues.**Other:**More words follow.",
        "**Label:** Body continues here.",
        "**Label**: Body continues here.",
        "A * B and C _ D.",
        "tail*",
        "_",
        "some_name_here",
        "**Label🫠**Body continues here.",
    ] {
        let (output, requests) = translated_source(source).await;
        assert!(
            requests.iter().all(|request| !request.contains(['*', '_'])),
            "literal delimiters must not reach the model: {requests:?}"
        );
        assert_eq!(source.matches('*').count(), output.matches('*').count());
        assert_eq!(source.matches('_').count(), output.matches('_').count());
        assert_eq!(markdown_structure(source), markdown_structure(&output));
        for flank in [":**", "：**", ":__", "🫠**"] {
            if source.contains(flank) && markdown_structure(source).inline.is_empty() {
                assert!(
                    output.contains(flank),
                    "source punctuation must stay adjacent"
                );
            }
        }
        if source.contains("Body") {
            assert!(output.contains("译"), "text must still translate");
            assert!(!output.contains("Label") && !output.contains("Body"));
        }
    }
}

#[test]
fn literal_delimiter_ownership_keeps_bare_urls_intact() {
    let config = make_stream_config("http://127.0.0.1:9/v1");
    let segmenter = fallback_segmenter();
    for url in [
        "https://example.test/some_path",
        "https://example.test/search?q=prefix*",
    ] {
        let source = format!("Read {url} for further details.");
        let plan = plan_translation(
            &source,
            "zh",
            &config,
            &segmenter,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        // A provider can preserve a URL only if its complete bytes reach one request.
        let translations: Vec<_> = plan
            .segments
            .iter()
            .map(|segment| {
                if segment.contains(url) {
                    format!("译文 {url} 译文")
                } else {
                    "译文".to_owned()
                }
            })
            .collect();
        assert!(
            plan.reconstruct(&translations).contains(url),
            "URL was split: {url}"
        );
    }
}

#[tokio::test]
async fn table_cells_translate_without_repairing_source_layout() {
    for source in [
        "Header one |\nHeader two |\n|---|---|\nRow one |\n| Value one | Value two |\n",
        "Header one |\nHeader two |\n| Value one | Value two |\n",
        "Header one | Header two\n--- | ---\nValue one | Value two\nRagged one\nRagged two\n",
        "| Header one | Header two |\r\n| :--- | ---: |\r\n| Value one | Value two |\r\n",
        "| [Visible label](https://example.com/a|b) | `keep | code` |\n| --- | --- |\n| Value one | Value two |\n",
    ] {
        let (output, requests) = translated_source(source).await;
        assert_eq!(markdown_structure(source), markdown_structure(&output));
        let layout = |text: &str| text.chars().filter(|c| matches!(c, '|' | '\n' | '\r')).collect::<String>();
        assert_eq!(layout(source), layout(&output), "source table boundaries must survive");
        assert!(!requests.is_empty());
        assert!(requests.iter().all(|request| !request.contains('|') && !request.contains('\n')),
            "only cell text may reach the provider: {requests:?}");
        assert!(output.contains('译'));
        assert!(!output.contains("Header") && !output.contains("Value") && !output.contains("Ragged") && !output.contains("Visible"));
    }
}

#[tokio::test]
async fn all_urls_remain_source_owned_while_adjacent_prose_translates() {
    for source in [
        "https://example.com/plain-path\n",
        "[https://example.com/plain-path](https://example.com/plain-path)\n",
        "Before https://example.com/plain-path, after.\n",
        "Before https://example.com/some_path?q=prefix* after.\n",
        "[Visible label](https://example.com/plain-path) after.\n",
    ] {
        let (output, requests) = translated_source(source).await;
        assert_eq!(
            hymt_core::completeness::preserved_urls(source),
            hymt_core::completeness::preserved_urls(&output)
        );
        assert!(requests.iter().all(|request| !request.contains("https://")));
        assert_eq!(markdown_structure(source), markdown_structure(&output));
        if source.contains("Before") || source.contains("Visible") {
            assert!(!requests.is_empty());
            assert!(output.contains('译'));
        } else {
            assert!(requests.is_empty());
            assert_eq!(output, source);
        }
    }
}

#[tokio::test]
async fn adjacent_url_owners_leave_prose_translatable() {
    let config = make_stream_config("http://127.0.0.1:9/v1");
    let segmenter = fallback_segmenter();
    let opts = PromptOpts {
        document_translation_policy: Some(DocumentTranslationPolicy::TranslateAll),
        ..PromptOpts::default()
    };
    for source in [
        "|https://example.test/x|Visible|\n|---|---|\n",
        "| https://example.test/x | Visible |\n| --- | --- |\n",
        "https://example.test/x**Visible**",
        "https://example.test/x **Visible**",
        "https://example.test/x*Visible*",
        "https://example.test/x`kept`Visible",
        "https://example.test/x<span>Visible</span>",
        "https://example.test/x|Visible",
        "https://example.test/x**Visible**.",
        "https://example.test/x*Visible*，",
        "https://example.test/x**Visible**，Body",
        "https://example.test/x**Visible**, Body",
        "Body ，_Visible_",
        "Body，_Visible_",
        "_Visible_，Body",
        "|https://example.test/x**Visible**|Other|\n|---|---|\n",
        "https://example.test/x**Visible**[Other](https://other.test)",
        "https://example.test/x**Visible**)Other",
        "https://example.test/a_b_(c) **Visible**",
        "https://example.test/x[Visible](https://example.test/a*b*c|d)",
        "https://example.test/x![Visible](https://example.test/a*b*c|d)",
    ] {
        let plan = plan_translation(
            source,
            "zh",
            &config,
            &segmenter,
            &TemplateType::Default,
            &opts,
        )
        .unwrap();
        assert!(
            plan.segments
                .iter()
                .any(|segment| segment.contains("Visible")),
            "adjacent prose must enter the real planner: {source:?}: {:?}",
            plan.segments
        );
        assert_eq!(plan.reconstruct(&plan.segments), source);
        let (output, requests) = translated_source(source).await;
        assert!(requests.iter().any(|request| request.contains("Visible")));
        assert!(requests.iter().all(|request| !request.contains("https://")));
        assert!(!output.contains("Visible"), "{source:?}: {output:?}");
        assert!(output.contains('译'));
        assert_eq!(
            hymt_core::completeness::preserved_urls(source),
            hymt_core::completeness::preserved_urls(&output)
        );
        assert_eq!(markdown_structure(source), markdown_structure(&output));
        if matches!(source, "Body，_Visible_" | "Body ，_Visible_") {
            assert_eq!(output, "译译译 _译译译译_");
        } else if source == "_Visible_，Body" {
            assert_eq!(output, "_译译译译_ 译译译");
        }
        if source.contains("`kept`") {
            assert!(output.contains("`kept`"));
        }
    }
}

#[tokio::test]
async fn paragraph_boundaries_are_source_owned() {
    let source = "First paragraph contains the first factual claim.\n\nSecond paragraph contains a different factual claim.\n\nThird paragraph contains the final factual claim.\n";
    let (output, requests) = translated_source(source).await;
    assert_eq!(
        requests.len(),
        3,
        "each paragraph needs an independently validated translation"
    );
    assert!(requests.iter().all(|request| !request.contains("\n\n")));
    assert_eq!(output.matches("\n\n").count(), 2);
    for label in ["First", "Second", "Third"] {
        assert!(requests.iter().any(|request| request.contains(label)));
        assert!(!output.contains(label));
    }
}

#[test]
fn adjacent_url_markup_keeps_complete_link_labels_translatable() {
    let config = make_stream_config("http://127.0.0.1:1");
    let segmenter = fallback_segmenter();
    for (source, label) in [
        ("__https://example.test/repo-one__[visible words before the next link](https://example.test/repo-two)", "visible words before the next link"),
        ("__https://example.test/repo-two__[Figure A](#fig:sample)and all of", "Figure A"),
        ("https://example.com/path[visible label](https://example.net/next)", "visible label"),
    ] {
        let plan = plan_translation(source, "fr", &config, &segmenter, &TemplateType::Default, &PromptOpts::default()).unwrap();
        assert!(plan.segments.iter().any(|segment| segment.contains(label)), "link label swallowed: {:?}", plan.segments);
        let output = plan.reconstruct(&vec!["Traduction".to_owned(); plan.segment_count()]);
        assert!(!output.contains(label));
    }
}
