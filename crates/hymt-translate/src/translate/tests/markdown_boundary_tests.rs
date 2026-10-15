use super::super::markdown_boundary::{block_structure, render_translations, take_parse_work};
use super::*;

#[test]
fn independent_emphasis_repairs_do_not_reparse_the_document_per_paragraph() {
    let config = make_stream_config("http://127.0.0.1:1");
    let segmenter = fallback_segmenter();
    let mut previous = None;
    for count in [64, 128] {
        let source = "a*Title*b\n\n".repeat(count);
        let plan = plan_translation(
            &source,
            "zh",
            &config,
            &segmenter,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        assert_eq!(plan.segments.len(), 3 * count);
        let mut leaves = ["甲", "《标题》", "乙\n\n"]
            .into_iter()
            .cycle()
            .take(3 * count)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        take_parse_work();
        assert!(render_translations(&plan, &mut leaves));
        let (calls, bytes) = take_parse_work();
        let output = plan.reconstruct(&leaves);
        ensure_markdown_structure_preserved(&source, &output).unwrap();
        if let Some((smaller_calls, smaller_bytes)) = previous {
            assert!(
                calls < 3 * smaller_calls,
                "parse calls grew from {smaller_calls} to {calls}"
            );
            assert!(
                bytes < 3 * smaller_bytes,
                "parser input bytes grew from {smaller_bytes} to {bytes}"
            );
        }
        previous = Some((calls, bytes));
    }
}

#[test]
fn independent_spans_in_one_paragraph_use_local_owner_windows() {
    let config = make_stream_config("http://127.0.0.1:1");
    let segmenter = fallback_segmenter();
    let mut previous = None;
    for count in [32, 64] {
        let source = "a*Title*b ".repeat(count);
        let plan = plan_translation(
            &source,
            "zh",
            &config,
            &segmenter,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        let mut leaves = plan
            .segments
            .iter()
            .map(|text| {
                text.replace("Title", "《标题》")
                    .replace('a', "甲")
                    .replace('b', "乙")
            })
            .collect::<Vec<_>>();
        take_parse_work();
        assert!(render_translations(&plan, &mut leaves));
        let (calls, bytes) = take_parse_work();
        let output = plan.reconstruct(&leaves);
        ensure_markdown_structure_preserved(&source, &output).unwrap();
        assert_eq!(output, "甲 *《标题》* 乙 ".repeat(count));
        if let Some((smaller_calls, smaller_bytes)) = previous {
            assert!(
                calls < 3 * smaller_calls,
                "parse calls grew from {smaller_calls} to {calls}"
            );
            assert!(
                bytes < 3 * smaller_bytes,
                "parser input bytes grew from {smaller_bytes} to {bytes}"
            );
        }
        previous = Some((calls, bytes));
    }
}

#[test]
fn mixed_ascii_and_nonbreaking_repairs_keep_each_original_leaf() {
    let config = make_stream_config("http://127.0.0.1:1");
    let segmenter = fallback_segmenter();
    let source = "a*Title*b\n\n*Title.*Journal\n\n".repeat(24);
    let plan = plan_translation(
        &source,
        "zh",
        &config,
        &segmenter,
        &TemplateType::Default,
        &PromptOpts::default(),
    )
    .unwrap();
    let mut leaves = plan
        .segments
        .iter()
        .map(|text| {
            text.replace("Title", "标题")
                .replace("Journal", "《期刊》")
                .replace('a', "甲")
                .replace('b', "乙")
        })
        .collect::<Vec<_>>();
    assert!(render_translations(&plan, &mut leaves));
    let output = plan.reconstruct(&leaves);
    assert_eq!(output, "甲*标题*乙\n\n*\u{a0}标题.*《期刊》\n\n".repeat(24));
    ensure_markdown_structure_preserved(&source, &output).unwrap();
}

#[test]
fn parser_owned_delimiters_survive_translated_flanks_without_rewriting_leaves() {
    let config = make_stream_config("http://127.0.0.1:1");
    let segmenter = fallback_segmenter();
    for (source, leaves, expected) in [
        (
            "a*Title*b",
            vec!["甲", "《标题》", "乙"],
            "甲 *《标题》* 乙",
        ),
        ("*Title*58", vec!["《标题》", "58"], "*《标题》* 58"),
        ("*condition*, next", vec!["条件。", "后续"], "*条件。* 后续"),
        (
            "**H100-equivalent (H100e)**, taken",
            vec!["H100同等性能（H100e）", "按照"],
            "**H100同等性能（H100e）** 按照",
        ),
        ("__Title__ next", vec!["标题", "后续"], "__标题__ 后续"),
        (
            "*Title.*Journal",
            vec!["标题", "《期刊》"],
            "*\u{a0}标题.*《期刊》",
        ),
    ] {
        let plan = plan_translation(
            source,
            "zh",
            &config,
            &segmenter,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        assert_eq!(plan.segments.len(), leaves.len(), "{source}");
        let mut translated: Vec<String> = leaves.iter().map(|s| (*s).to_owned()).collect();
        assert!(render_translations(&plan, &mut translated), "{source}");
        let output = plan.reconstruct(&translated);
        assert_eq!(output, expected, "{source}");
        ensure_markdown_structure_preserved(source, &output).unwrap();
        assert_eq!(block_structure(source), block_structure(&output));
        for (before, after) in leaves.iter().zip(&translated) {
            assert!(
                after == before
                    || after.strip_prefix(' ').is_some_and(|rest| rest == *before)
                    || after.strip_suffix(' ').is_some_and(|rest| rest == *before)
                    || after
                        .strip_prefix('\u{a0}')
                        .is_some_and(|rest| rest == *before)
                    || after
                        .strip_suffix('\u{a0}')
                        .is_some_and(|rest| rest == *before),
                "only one edge separator may be inserted per translated leaf"
            );
        }
        let once = translated.clone();
        assert!(render_translations(&plan, &mut translated));
        assert_eq!(translated, once, "rendering must be idempotent");
    }
}

#[test]
fn two_spans_each_repair_both_flanks_without_touching_middle_content() {
    let source = "a*Title*b c*Other*d";
    let config = make_stream_config("http://127.0.0.1:1");
    let segmenter = fallback_segmenter();
    let plan = plan_translation(
        source,
        "zh",
        &config,
        &segmenter,
        &TemplateType::Default,
        &PromptOpts::default(),
    )
    .unwrap();
    assert_eq!(plan.segments, ["a", "Title", "b c", "Other", "d"]);
    let mut leaves = vec!["甲", "《标题》", "乙 丙", "《其他》", "丁"]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert!(render_translations(&plan, &mut leaves));
    let output = plan.reconstruct(&leaves);
    assert_eq!(output, "甲 *《标题》* 乙 丙 *《其他》* 丁");
    ensure_markdown_structure_preserved(source, &output).unwrap();
}

#[test]
fn unrelated_model_markup_and_block_changes_are_rejected() {
    let config = make_stream_config("http://127.0.0.1:1");
    let segmenter = fallback_segmenter();
    let plan = plan_translation(
        "*Title*58",
        "zh",
        &config,
        &segmenter,
        &TemplateType::Default,
        &PromptOpts::default(),
    )
    .unwrap();
    let mut leaves = vec!["**标题**".to_owned(), "58".to_owned()];
    assert!(!render_translations(&plan, &mut leaves));
    for source in ["*Title.*Journal", "> *Title.*Journal", "- *Title.*Journal"] {
        let plan = plan_translation(
            source,
            "zh",
            &config,
            &segmenter,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        let mut leaves: Vec<_> = plan
            .segments
            .iter()
            .map(|text| text.replace("Title", "标题").replace("Journal", "《期刊》"))
            .collect();
        assert!(render_translations(&plan, &mut leaves));
        let output = plan.reconstruct(&leaves);
        assert_eq!(block_structure(source), block_structure(&output));
        ensure_markdown_structure_preserved(source, &output).unwrap();
    }
}

#[tokio::test]
async fn rendered_document_is_identical_in_buffered_streamed_and_cached_paths() {
    for mode in [
        None,
        Some(StreamOutputMode::Validated),
        Some(StreamOutputMode::Optimistic),
    ] {
        let source = "*Title*58";
        let expected = "*《标题》* 58";
        let server = start_capturing_mock_server(vec![
            MockResponse::Json("《标题》".to_owned()),
            MockResponse::Json("58".to_owned()),
        ])
        .await;
        let config = make_stream_config_with_fcp(&server.endpoint_url, false);
        let client = TranslationClient::new(config.clone()).unwrap();
        let segmenter = fallback_segmenter();
        let dir = tempfile::tempdir().unwrap();
        let history = HistoryDB::new(dir.path().join("history.db"));
        let ctx = TranslationCtx {
            config: &config,
            client: &client,
            segmenter: &segmenter,
            history: &history,
            cache_enabled: true,
        };
        let opts = PromptOpts::default();
        if let Some(mode) = mode {
            let (tx, mut rx) = mpsc::channel(32);
            let outcome = translate_text_stream_with_mode(
                source,
                "zh",
                &TemplateType::Default,
                &opts,
                &ctx,
                mode,
                tx,
            )
            .await
            .unwrap();
            let mut tokens = String::new();
            let mut done = None;
            while let Some(event) = rx.recv().await {
                match event {
                    StreamEvent::Token(text) => tokens.push_str(&text),
                    StreamEvent::AllDone(text) => done = Some(text),
                    _ => {}
                }
            }
            assert_eq!(tokens, expected);
            assert_eq!(done.as_deref(), Some(expected));
            assert_eq!(outcome.text, expected);
        } else {
            assert_eq!(
                translate_text(source, "zh", &TemplateType::Default, &opts, &ctx)
                    .await
                    .unwrap()
                    .text,
                expected
            );
        }
        assert_eq!(server.requests.lock().unwrap().len(), 2);
        assert_eq!(
            translate_text(source, "zh", &TemplateType::Default, &opts, &ctx)
                .await
                .unwrap()
                .text,
            expected
        );
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }
}
