use super::super::markdown_boundary::{
    block_structure, render_translations, take_parse_work, take_render_work, RenderWork,
};
use super::*;

thread_local! {
    static SEMANTIC_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(in super::super) fn record_semantic_bytes(bytes: usize) {
    SEMANTIC_BYTES.with(|total| total.set(total.get() + bytes));
}

#[test]
fn generated_inline_code_recovery_appends_each_run_without_prefix_copies() {
    let mut work = Vec::new();
    for count in [64, 128] {
        let reply = format!("a {}", "`x` ".repeat(count));
        SEMANTIC_BYTES.with(|total| total.set(0));
        assert_eq!(
            recover_generated_inline_code("Plain source text.", &reply),
            Some(format!("a {}", "x ".repeat(count)))
        );
        let bytes = SEMANTIC_BYTES.with(std::cell::Cell::get);
        eprintln!(
            "semantic recovery n={count} reply_bytes={} copied_or_appended_bytes={bytes}",
            reply.len()
        );
        work.push((bytes, reply.len()));
    }
    assert!(
        work[1].0 < 3 * work[0].0,
        "prefix copying must not grow quadratically: {work:?}"
    );
    assert!(
        work.iter().all(|(bytes, input)| *bytes <= 4 * input),
        "each decoded run must append linearly: {work:?}"
    );
}

fn assert_work_growth(smaller: RenderWork, larger: RenderWork) {
    eprintln!("renderer work: {smaller:?} -> {larger:?}");
    assert!(
        larger.joined_bytes < 3 * smaller.joined_bytes,
        "joined bytes: {smaller:?} -> {larger:?}"
    );
    assert!(
        larger.copied_bytes < 3 * smaller.copied_bytes,
        "copied bytes: {smaller:?} -> {larger:?}"
    );
    assert!(
        larger.member_visits < 3 * smaller.member_visits,
        "member visits: {smaller:?} -> {larger:?}"
    );
    assert!(
        larger.invalidation_queries < 3 * smaller.invalidation_queries,
        "invalidation queries: {smaller:?} -> {larger:?}"
    );
}

fn strong_span_count(text: &str) -> usize {
    pulldown_cmark::Parser::new(text)
        .filter(|event| {
            matches!(
                event,
                pulldown_cmark::Event::Start(pulldown_cmark::Tag::Strong)
            )
        })
        .count()
}

#[test]
fn identical_nested_strong_spans_settle_without_repeated_parse() {
    let config = make_stream_config("http://127.0.0.1:1");
    let segmenter = fallback_segmenter();
    let mut previous = None;
    let mut previous_work = None;
    for count in [64, 128] {
        let source = format!("a{}Title{}b", "**".repeat(count), "**".repeat(count));
        assert_eq!(strong_span_count(&source), count);
        let plan = plan_translation(
            &source,
            "zh",
            &config,
            &segmenter,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        assert_eq!(plan.segments, ["a", "Title", "b"]);
        assert_eq!(plan.segment_section_groups.len(), 3);
        assert_eq!(plan.reconstruct(&plan.segments), source);
        assert_eq!(
            plan.document_plan
                .as_ref()
                .unwrap()
                .sections
                .iter()
                .filter(|section| section.text.contains("**"))
                .count(),
            2
        );
        let mut leaves = vec!["甲".to_owned(), "《标题》".to_owned(), "乙".to_owned()];
        take_parse_work();
        take_render_work();
        assert!(render_translations(&plan, &mut leaves));
        let work = take_parse_work();
        let render_work = take_render_work();
        let output = plan.reconstruct(&leaves);
        ensure_markdown_structure_preserved(&source, &output).unwrap();
        assert_eq!(leaves, ["甲 ", "《标题》", " 乙"]);
        if let Some((small_calls, small_bytes)) = previous {
            assert!(
                work.0 < 3 * small_calls,
                "parse calls: {small_calls} -> {}",
                work.0
            );
            assert!(
                work.1 < 3 * small_bytes,
                "parser bytes: {small_bytes} -> {}",
                work.1
            );
        }
        if let Some(smaller) = previous_work {
            assert_work_growth(smaller, render_work);
        }
        previous = Some(work);
        previous_work = Some(render_work);
    }
}

#[test]
fn distributed_nested_strong_spans_settle_without_repeated_parse() {
    let config = make_stream_config("http://127.0.0.1:1");
    let segmenter = fallback_segmenter();
    let mut previous = None;
    let mut previous_work = None;
    for count in [63, 126] {
        let source = format!("a{}Title{}", "**".repeat(count), "** b".repeat(count));
        assert_eq!(strong_span_count(&source), count);
        let plan = plan_translation(
            &source,
            "zh",
            &config,
            &segmenter,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        assert_eq!(plan.segments.len(), count + 2);
        assert_eq!(plan.segments[0], "a");
        assert_eq!(plan.segments[1], "Title");
        assert_eq!(plan.reconstruct(&plan.segments), source);
        assert_eq!(
            plan.document_plan
                .as_ref()
                .unwrap()
                .sections
                .iter()
                .filter(|section| section.text.contains("**"))
                .count(),
            count + 1
        );
        let mut leaves = plan
            .segments
            .iter()
            .map(|s| {
                s.replace('a', "甲")
                    .replace("Title", "《标题》")
                    .replace('b', "乙")
            })
            .collect::<Vec<_>>();
        take_parse_work();
        take_render_work();
        assert!(render_translations(&plan, &mut leaves));
        let work = take_parse_work();
        let render_work = take_render_work();
        let output = plan.reconstruct(&leaves);
        ensure_markdown_structure_preserved(&source, &output).unwrap();
        assert_eq!(leaves[0], "甲 ");
        assert_eq!(leaves[1], "《标题》");
        if let Some((small_calls, small_bytes)) = previous {
            assert!(
                work.0 < 3 * small_calls,
                "parse calls: {small_calls} -> {}",
                work.0
            );
            assert!(
                work.1 < 3 * small_bytes,
                "parser bytes: {small_bytes} -> {}",
                work.1
            );
        }
        if let Some(smaller) = previous_work {
            assert_work_growth(smaller, render_work);
        }
        previous = Some(work);
        previous_work = Some(render_work);
    }
}

#[test]
fn separate_nested_groups_each_repair_without_repeated_parse() {
    let config = make_stream_config("http://127.0.0.1:1");
    let segmenter = fallback_segmenter();
    let mut previous = None;
    let mut previous_work = None;
    for count in [32, 64] {
        let group = format!("a{}Title{}b\n\n", "**".repeat(count), "**".repeat(count));
        let source = group.repeat(4);
        assert_eq!(strong_span_count(&source), 4 * count);
        let plan = plan_translation(
            &source,
            "zh",
            &config,
            &segmenter,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        assert_eq!(plan.segments.len(), 12);
        assert_eq!(plan.reconstruct(&plan.segments), source);
        let mut leaves = plan
            .segments
            .iter()
            .map(|s| {
                s.replace('a', "甲")
                    .replace("Title", "《标题》")
                    .replace('b', "乙")
            })
            .collect::<Vec<_>>();
        take_parse_work();
        take_render_work();
        assert!(render_translations(&plan, &mut leaves));
        let work = take_parse_work();
        let render_work = take_render_work();
        let output = plan.reconstruct(&leaves);
        ensure_markdown_structure_preserved(&source, &output).unwrap();
        assert_eq!(output.matches("甲 ").count(), 4);
        if let Some((small_calls, small_bytes)) = previous {
            assert!(
                work.0 < 3 * small_calls,
                "parse calls: {small_calls} -> {}",
                work.0
            );
            assert!(
                work.1 < 3 * small_bytes,
                "parser bytes: {small_bytes} -> {}",
                work.1
            );
        }
        if let Some(smaller) = previous_work {
            assert_work_growth(smaller, render_work);
        }
        previous = Some(work);
        previous_work = Some(render_work);
    }
}

#[test]
fn separate_distributed_nested_groups_settle_locally() {
    let config = make_stream_config("http://127.0.0.1:1");
    let segmenter = fallback_segmenter();
    let mut previous = None;
    let mut previous_work = None;
    for count in [30, 60] {
        let group = format!("a{}Title{}\n\n", "**".repeat(count), "** b".repeat(count));
        let source = group.repeat(4);
        assert_eq!(strong_span_count(&source), 4 * count);
        let plan = plan_translation(
            &source,
            "zh",
            &config,
            &segmenter,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        assert_eq!(plan.segments.len(), 4 * (count + 2));
        assert_eq!(plan.reconstruct(&plan.segments), source);
        let mut leaves = plan
            .segments
            .iter()
            .map(|s| {
                s.replace('a', "甲")
                    .replace("Title", "《标题》")
                    .replace('b', "乙")
            })
            .collect::<Vec<_>>();
        take_parse_work();
        take_render_work();
        assert!(render_translations(&plan, &mut leaves));
        let work = take_parse_work();
        let render_work = take_render_work();
        let output = plan.reconstruct(&leaves);
        ensure_markdown_structure_preserved(&source, &output).unwrap();
        assert_eq!(output.matches("甲 ").count(), 4);
        if let Some((small_calls, small_bytes)) = previous {
            assert!(
                work.0 < 3 * small_calls,
                "parse calls: {small_calls} -> {}",
                work.0
            );
            assert!(
                work.1 < 3 * small_bytes,
                "parser bytes: {small_bytes} -> {}",
                work.1
            );
        }
        if let Some(smaller) = previous_work {
            assert_work_growth(smaller, render_work);
        }
        previous = Some(work);
        previous_work = Some(render_work);
    }
}

#[test]
fn covered_empty_section_and_split_group_keep_exact_owners() {
    let source = "a*Title*b";
    let config = make_stream_config("http://127.0.0.1:1");
    let segmenter = fallback_segmenter();
    let mut plan = plan_translation(
        source,
        "zh",
        &config,
        &segmenter,
        &TemplateType::Default,
        &PromptOpts::default(),
    )
    .unwrap();
    assert_eq!(plan.segments, ["a", "Title", "b"]);
    {
        let document = plan.document_plan.as_mut().unwrap();
        assert_eq!(
            document
                .sections
                .iter()
                .map(|section| section.text.as_str())
                .collect::<Vec<_>>(),
            ["a", "*", "Title", "*", "b"]
        );
        document.sections[2].text = "Ti".to_owned();
        let mut covered = document.sections[2].clone();
        covered.text = "tle".to_owned();
        document.sections.insert(3, covered);
    }
    plan.segments = ["a", "Ti", "tle", "b"].map(str::to_owned).to_vec();
    plan.segment_section_indexes = vec![0, 2, 2, 5];
    plan.segment_section_groups = vec![vec![0], vec![2, 3], vec![2, 3], vec![5]];
    assert_eq!(plan.reconstruct(&plan.segments), source);
    assert_eq!(
        reconstructed_section_parts(
            plan.document_plan.as_ref().unwrap(),
            &plan.segment_section_groups,
            &plan.segments,
            &plan.segments
        ),
        ["a", "*", "Title", "", "*", "b"]
    );
    let mut leaves = ["甲", "《标", "题》", "乙"].map(str::to_owned).to_vec();
    assert!(render_translations(&plan, &mut leaves));
    assert_eq!(leaves, ["甲 ", "《标", "题》", " 乙"]);
    let output = plan.reconstruct(&leaves);
    assert_eq!(output, "甲 *《标题》* 乙");
    ensure_markdown_structure_preserved(source, &output).unwrap();
}

#[test]
fn independent_emphasis_repairs_do_not_reparse_the_document_per_paragraph() {
    let config = make_stream_config("http://127.0.0.1:1");
    let segmenter = fallback_segmenter();
    let mut previous = None;
    let mut previous_work = None;
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
        take_render_work();
        assert!(render_translations(&plan, &mut leaves));
        let (calls, bytes) = take_parse_work();
        let render_work = take_render_work();
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
        if let Some(smaller) = previous_work {
            assert_work_growth(smaller, render_work);
        }
        previous = Some((calls, bytes));
        previous_work = Some(render_work);
    }
}

#[test]
fn independent_spans_in_one_paragraph_use_local_owner_windows() {
    let config = make_stream_config("http://127.0.0.1:1");
    let segmenter = fallback_segmenter();
    let mut previous = None;
    let mut previous_work = None;
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
        take_render_work();
        assert!(render_translations(&plan, &mut leaves));
        let (calls, bytes) = take_parse_work();
        let render_work = take_render_work();
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
        if let Some(smaller) = previous_work {
            assert_work_growth(smaller, render_work);
        }
        previous = Some((calls, bytes));
        previous_work = Some(render_work);
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
        let config = leaf_fixture_config(
            &make_stream_config_with_fcp(&server.endpoint_url, false),
            source,
        );
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

#[test]
fn shared_block_guard_checks_empty_spans_html_and_atomic_json() {
    let config = make_stream_config("http://127.0.0.1:9/v1");
    let segmenter = fallback_segmenter();
    let opts = PromptOpts::default();
    let prose = "30 Read the complete synthetic instruction.";
    let mut admitted = Vec::new();
    for (source, candidate) in [
        (prose, "30. 请完整阅读这一条合成操作说明。"),
        (prose, "<span>30 请完整阅读这一条合成操作说明。</span>"),
        (
            "Read <span class=\"original\">the complete instruction</span>.",
            "阅读 <span class=\"changed\">这条完整的操作说明</span>。",
        ),
    ] {
        if ensure_markdown_structure_preserved(source, candidate).is_ok() {
            admitted.push((source, "shared guard"));
        }
        if source == prose {
            let plan = plan_translation(
                source,
                "zh",
                &config,
                &segmenter,
                &TemplateType::Default,
                &opts,
            )
            .unwrap();
            assert_eq!(plan.segments, [source]);
            let mut leaves = vec![candidate.to_owned()];
            if render_translations(&plan, &mut leaves) {
                admitted.push((source, "empty-span renderer"));
            }
            assert_eq!(
                leaves,
                [candidate],
                "rejected rendering must not rewrite text"
            );
        }
    }
    for (source, candidate) in [
        (
            "30. Read the complete synthetic instruction.",
            "30. 请完整阅读这一条合成操作说明。",
        ),
        (
            "Read <span class=\"original\">the complete instruction</span>.",
            "阅读 <span class=\"original\">这条完整的操作说明</span>。",
        ),
    ] {
        ensure_markdown_structure_preserved(source, candidate).unwrap();
    }
    let source = "{\n\n    \"message\": \"Read the complete instruction.\",\n    \"count\": 2,\n    \"items\": [\"First\"]\n}";
    let valid = r#"{"message":"请完整阅读这一条操作说明。","count":2,"items":["第一项"]}"#;
    let plan = plan_translation(
        source,
        "zh",
        &config,
        &segmenter,
        &TemplateType::Default,
        &opts,
    )
    .unwrap();
    assert_eq!(plan.segments, [source]);
    ensure_markdown_structure_preserved(source, valid).unwrap();
    assert!(render_translations(&plan, &mut [valid.to_owned()]));
    for candidate in [
        "not JSON",
        r#"{"wrong":"请完整阅读这一条操作说明。","count":2,"items":["第一项"]}"#,
        r#"{"message":"请完整阅读这一条操作说明。","count":3,"items":["第一项"]}"#,
        r#"{"message":"请完整阅读这一条操作说明。","count":2,"items":["第一项","额外"]}"#,
    ] {
        if ensure_markdown_structure_preserved(source, candidate).is_ok() {
            admitted.push((candidate, "JSON shared guard"));
        }
        if render_translations(&plan, &mut [candidate.to_owned()]) {
            admitted.push((candidate, "JSON renderer"));
        }
    }
    assert!(admitted.is_empty(), "changed shape admitted: {admitted:?}");
}

fn plan_translation(
    source: &str,
    target: &str,
    config: &HotConfig,
    segmenter: &Segmenter,
    template: &TemplateType,
    opts: &PromptOpts,
) -> Result<TranslationPlan> {
    super::super::plan_translation(
        source,
        target,
        &leaf_fixture_config(config, source),
        segmenter,
        template,
        opts,
    )
}
