use super::*;

#[tokio::test]
async fn settled_owner_publishes_trailing_protected_literal_before_next_owner() {
    let table = format!(
        "| Header | Detail |\n| --- | --- |\n| [guide] | ![icon][] {}|\n",
        "padding ".repeat(300)
    );
    let source = format!(
        "Before marker.\n{table}\nLater owner paragraph.\n\n[guide]: https://example.test/guide\n[icon]: https://example.test/icon.png\n"
    );
    let config = make_stream_config("http://127.0.0.1:1/v1");
    let plan = plan_translation(
        &source,
        "zh",
        &config,
        &fallback_segmenter(),
        &TemplateType::Default,
        &PromptOpts::default(),
    )
    .unwrap();
    assert_eq!(plan.segments, ["Before marker.", "Later owner paragraph."]);
    let (tx, mut rx) = mpsc::channel(64);
    let mut stream = paragraph_stream::ParagraphStream::new(&source, &plan, tx);
    let translations = vec![Some("FIRST_REPLY".to_owned()), None];
    stream.flush(&translations, Some((0, false))).await.unwrap();
    let mut prefix = String::new();
    while let Ok(event) = rx.try_recv() {
        if let StreamEvent::Token(text) = event {
            prefix.push_str(&text);
        }
    }
    assert!(prefix.contains("FIRST_REPLY"));
    assert!(
        prefix.contains(&table),
        "settled owner is missing its literal tail"
    );
    assert!(!prefix.contains("Later owner"));
}

#[tokio::test]
async fn split_owner_readiness_work_grows_linearly_for_completion_order() {
    let source = format!("{}\n", "Source words stay in one paragraph. ".repeat(128));
    let config = make_stream_config("http://127.0.0.1:1/v1");
    let plan = plan_translation(
        &source,
        "zh",
        &config,
        &fallback_segmenter(),
        &TemplateType::Default,
        &PromptOpts::default(),
    )
    .unwrap();
    let pieces = plan.segment_count();
    assert!(pieces >= 8);
    for reversed in [false, true] {
        let (tx, _rx) = mpsc::channel(4 * pieces + 4);
        let mut stream = paragraph_stream::ParagraphStream::new(&source, &plan, tx);
        let mut translations = vec![None; pieces];
        let order: Vec<_> = if reversed {
            (0..pieces).rev().collect()
        } else {
            (0..pieces).collect()
        };
        for index in order {
            translations[index] = Some(format!("REPLY_{index}"));
            stream
                .flush(&translations, Some((index, false)))
                .await
                .unwrap();
        }
        assert_eq!(stream.next_segment, pieces);
        assert!(
            stream.owner_reads + stream.readiness_reads <= pieces * 4,
            "{} owner reads and {} readiness reads for {pieces} pieces, reversed={reversed}",
            stream.owner_reads,
            stream.readiness_reads,
        );
    }
}

#[test]
fn split_paragraph_newline_reads_grow_linearly() {
    struct Counted<'a>(&'a std::cell::Cell<usize>);
    impl AsRef<str> for Counted<'_> {
        fn as_ref(&self) -> &str {
            self.0.set(self.0.get() + 1);
            "translated reply"
        }
    }

    let config = make_stream_config("http://127.0.0.1:1/v1");
    for repeats in [32, 128] {
        let source = format!(
            "{}\n",
            "Source words stay in one paragraph. ".repeat(repeats)
        );
        let plan = plan_translation(
            &source,
            "zh",
            &config,
            &fallback_segmenter(),
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        let pieces = plan.segment_count();
        assert!(pieces >= 8);
        let reads = std::cell::Cell::new(0);
        let translations: Vec<_> = (0..pieces).map(|_| Some(Counted(&reads))).collect();
        for index in 0..pieces {
            reconstruction_newline_after_segment(&plan, &translations, index);
        }
        assert!(
            reads.get() <= pieces * 2,
            "{} reads for {pieces} pieces",
            reads.get()
        );
    }
}

#[tokio::test]
async fn paragraph_stream_fences_literals_and_replaces_a_failed_split_owner_once() {
    for prefix in ["`literal` ", "- `literal` ", "> `literal` "] {
        let failed = format!(
            "{prefix}[guide][ref] FAIL_OWNER {}",
            "Words stay inside this source paragraph. ".repeat(12)
        );
        let source = format!("---\ntitle: test\n---\n\nFirst complete paragraph.\n\n{failed}\n\nFinal complete paragraph.\n\n```text\ntrailing literal\n```\n\n[ref]: https://example.test/guide\n");
        let config = make_stream_config("http://127.0.0.1:1/v1");
        let plan = plan_translation(
            &source,
            "zh",
            &config,
            &fallback_segmenter(),
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        let original = plan
            .original_paragraphs
            .iter()
            .find(|paragraph| source[paragraph.bytes.clone()].contains("FAIL_OWNER"))
            .unwrap();
        let failed_indexes: Vec<_> = (0..plan.segment_count())
            .filter(|&index| retained_original::owner(&plan, index) == Some(original.index))
            .collect();
        assert!(
            failed_indexes.len() > 1,
            "fixture must split a complete original owner"
        );
        let first = failed_indexes[0];
        let end = failed_indexes.last().unwrap() + 1;
        let (tx, mut rx) = mpsc::channel(256);
        let mut stream = paragraph_stream::ParagraphStream::new(&source, &plan, tx);
        let mut translations = vec![None; plan.segment_count()];
        for (index, translation) in translations[..first].iter_mut().enumerate() {
            *translation = Some(format!("GOOD_{index}"));
        }
        stream.flush(&translations, None).await.unwrap();
        let mut emitted = String::new();
        while let Ok(event) = rx.try_recv() {
            if let StreamEvent::Token(text) = event {
                emitted.push_str(&text);
            }
        }
        assert!(emitted.contains("GOOD_0"));
        assert!(
            !emitted.contains("literal"),
            "future owner's literals escaped before its transaction completed"
        );
        translations[first] = Some("MUST_DISAPPEAR".into());
        stream
            .flush(&translations, Some((first, false)))
            .await
            .unwrap();
        assert!(
            rx.try_recv().is_err(),
            "a partial source paragraph was published"
        );
        let mut degraded = vec![first + 2];
        for index in first + 1..end {
            translations[index] = Some("DRAFT".into());
            stream
                .flush(&translations, Some((index, index + 1 == degraded[0])))
                .await
                .unwrap();
        }
        for index in end..translations.len() {
            translations[index] = Some(format!("LAST_{index}"));
            stream
                .flush(&translations, Some((index, false)))
                .await
                .unwrap();
        }
        let mut completed: Vec<_> = translations.into_iter().map(Option::unwrap).collect();
        let (final_text, regions) = retained_original::finalize_with_notice(
            &source,
            &plan,
            &mut completed,
            &mut degraded,
            true,
        )
        .unwrap();
        stream.finish(&final_text).await.unwrap();
        while let Some(event) = rx.recv().await {
            if let StreamEvent::Token(text) = event {
                emitted.push_str(&text);
            }
        }
        assert_eq!(emitted, final_text);
        assert_eq!(regions.len(), 1);
        assert_eq!(
            &emitted[regions[0].output_bytes.clone()],
            &source[original.bytes.clone()]
        );
        assert_eq!(emitted.matches(&source[original.bytes.clone()]).count(), 1);
        assert!(!emitted.contains("MUST_DISAPPEAR") && !emitted.contains("DRAFT"));
        assert!(
            emitted.find("Untranslated original:").unwrap()
                > emitted.find("trailing literal").unwrap()
        );
    }
}

#[tokio::test]
async fn paragraph_mode_transport_recovery_preserves_final_history_and_stdout() {
    let server = start_capturing_mock_server(vec![
        MockResponse::InterruptedSse(sse_token_body("unpublished draft")),
        MockResponse::Sse(vec!["complete reply".into()]),
    ])
    .await;
    let config = make_stream_config(&server.endpoint_url);
    let dir = tempfile::tempdir().unwrap();
    let history = HistoryDB::new(dir.path().join("history.db"));
    let (outcome, stdout) = translate_and_render_stdout_with_mode(
        "A complete source paragraph.",
        &config,
        &fallback_segmenter(),
        &history,
        StreamOutputMode::Paragraph,
    )
    .await
    .unwrap();
    assert_eq!(outcome.trim(), "complete reply");
    assert_eq!(stdout, "complete reply\n");
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
    assert_eq!(
        history.fetch_recent(None).unwrap()[0]
            .output_text
            .as_deref(),
        Some(outcome.as_str())
    );
}

#[tokio::test]
async fn split_paragraph_empty_replies_preserve_stream_and_history_bytes() {
    for mode in [StreamOutputMode::Paragraph, StreamOutputMode::Validated] {
        for (empty_pieces, tail, expected) in [
            (1, "", "FIRSTREPLY\n"),
            (2, "", "FIRSTREPLY\n"),
            (1, "\n", "FIRSTREPLY\n\n"),
            (2, "\n", "FIRSTREPLY\n\n\n"),
            (1, "\n\n", "FIRSTREPLY\n\n\n"),
            (1, "LASTREPLY", "FIRSTREPLY\nLASTREPLY\n"),
            (2, "LASTREPLY", "FIRSTREPLY\nLASTREPLY\nLASTREPLY\n"),
        ] {
            let source = format!(
                "FIRSTOWNER alpha zero\n{}",
                "SECONDOWNER bravo one\n".repeat(empty_pieces)
            );
            let mut replies = vec![MockResponse::Sse(vec!["FIRSTREPLY".into()])];
            let encoded = serde_json::to_string(tail).unwrap();
            replies.extend(
                (0..empty_pieces)
                    .map(|_| MockResponse::Json(encoded[1..encoded.len() - 1].to_owned())),
            );
            let server = start_capturing_mock_server(replies).await;
            let config = make_stream_config(&server.endpoint_url);
            let contents = std::fs::read_to_string(config.path()).unwrap().replace(
                "[translation]\n",
                "[translation]\nmax_source_tokens_per_segment = 8\n",
            );
            std::fs::write(config.path(), contents).unwrap();
            let config = HotConfig::from_path(config.path()).unwrap();
            let segmenter = fallback_segmenter();
            let directory = tempfile::tempdir().unwrap();
            let history = HistoryDB::new(directory.path().join("history.db"));
            let client = TranslationClient::new(config.clone()).unwrap();
            let context = TranslationCtx {
                config: &config,
                client: &client,
                segmenter: &segmenter,
                history: &history,
                cache_enabled: false,
            };
            let options = PromptOpts::default();
            let (tx, mut rx) = mpsc::channel(64);
            let translate = translate_text_stream_with_mode(
                &source,
                "zh",
                &TemplateType::Default,
                &options,
                &context,
                mode,
                tx,
            );
            let collect = async {
                let mut tokens = String::new();
                let mut terminal = None;
                while let Some(event) = rx.recv().await {
                    match event {
                        StreamEvent::Token(text) => tokens.push_str(&text),
                        StreamEvent::AllDone(text) => terminal = Some(text),
                        StreamEvent::SegmentDone(_) => {}
                    }
                }
                (tokens, terminal)
            };
            let (outcome, (tokens, terminal)) = tokio::join!(translate, collect);
            let outcome = outcome.unwrap();
            assert_eq!(outcome.text, expected);
            assert_eq!(tokens, outcome.text);
            assert_eq!(terminal.as_deref(), Some(tokens.as_str()));
            assert_eq!(
                history.fetch_recent(None).unwrap()[0]
                    .output_text
                    .as_deref(),
                Some(tokens.as_str())
            );
            let requests = server.requests.lock().unwrap();
            assert_eq!(requests.len(), empty_pieces + 1);
            let submitted: String = requests
                .iter()
                .map(|request| {
                    let payload: serde_json::Value =
                        serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                    payload["messages"][0]["content"]
                        .as_str()
                        .unwrap()
                        .split_once("\n\n")
                        .unwrap()
                        .1
                        .to_owned()
                })
                .collect();
            assert_eq!(submitted, source);
        }
    }
}
