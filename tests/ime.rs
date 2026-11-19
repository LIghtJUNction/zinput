use zinput::ime::{CandidatePage, Composition, ContextId, Error, Selection, TextTarget};

#[test]
fn lifecycle_preserves_multilingual_text_and_clears_composition() {
    let mut storage = [0; 128];
    let mut output = [0; 128];
    let context = ContextId::new(7).unwrap();
    let mut state = Composition::new(&mut storage);
    assert_eq!(state.begin(), Err(Error::NotFocused));
    assert!(state.set_focus(Some(context)));
    for text in ["中文", "かな", "한글", "العربية", "हिन्दी", "👩🏽‍💻", "e\u{301}"]
    {
        let token = state.begin().unwrap();
        let token = state
            .set_preedit(token, text, Selection::caret(text.len()))
            .unwrap();
        assert_eq!(state.preedit(), text);
        assert!(!state.set_focus(Some(context)));
        let commit = state.commit_preedit(token, &mut output).unwrap();
        assert_eq!(commit.context, context);
        assert_eq!(&output[..commit.bytes_written], text.as_bytes());
        assert_eq!(state.preedit(), "");
        assert_eq!(state.active_token(), None);
        assert_eq!(state.cancel(token), Err(Error::StaleToken));
    }
    drop(state);
    assert!(storage.iter().all(|byte| *byte == 0));
}

#[test]
fn failed_updates_are_atomic_at_capacity_and_utf8_boundaries() {
    let mut storage = [0; 6];
    let mut state = Composition::new(&mut storage);
    state.set_focus(Some(ContextId::new(1).unwrap()));
    let token = state.begin().unwrap();
    let token = state
        .set_preedit(token, "中文", Selection::caret(6))
        .unwrap();
    for selection in [
        Selection::caret(1),
        Selection { start: 6, end: 3 },
        Selection::caret(7),
    ] {
        assert_eq!(
            state.set_preedit(token, "中文", selection),
            Err(Error::InvalidSelection)
        );
        assert_eq!(state.preedit(), "中文");
        assert_eq!(state.active_token(), Some(token));
    }
    assert_eq!(
        state.set_preedit(token, "中文a", Selection::caret(7)),
        Err(Error::BufferTooSmall)
    );
    let mut short = [0xa5; 5];
    assert_eq!(
        state.commit_preedit(token, &mut short),
        Err(Error::BufferTooSmall)
    );
    assert_eq!(short, [0xa5; 5]);
    assert_eq!(state.preedit(), "中文");
    assert_eq!(state.selection(), Selection::caret(6));
    let mut exact = [0; 6];
    assert_eq!(
        state
            .commit_preedit(token, &mut exact)
            .unwrap()
            .bytes_written,
        6
    );
    assert_eq!(&exact, "中文".as_bytes());
}

#[test]
fn focus_and_revision_changes_reject_stale_candidates() {
    let mut storage = [0; 16];
    let mut output = [0xcc; 16];
    let first = ContextId::new(1).unwrap();
    let second = ContextId::new(2).unwrap();
    let mut state = Composition::new(&mut storage);
    state.set_focus(Some(first));
    let initial = state.begin().unwrap();
    let token = state
        .set_preedit(initial, "ni", Selection::caret(2))
        .unwrap();
    let page = CandidatePage::new(token, &["你", "呢"], Some(0)).unwrap();
    let newer = state
        .set_preedit(token, "nin", Selection::caret(3))
        .unwrap();
    assert_eq!(
        state.commit_candidate(&page, 0, &mut output),
        Err(Error::StaleToken)
    );
    assert_eq!(output, [0xcc; 16]);
    assert!(state.set_focus(Some(second)));
    assert_eq!(state.cancel(newer), Err(Error::StaleToken));
    assert_eq!(state.preedit(), "");
    state.set_focus(Some(first));
    let new_session = state.begin().unwrap();
    assert_ne!(new_session, initial);
    assert_eq!(
        state.commit(token, "你", &mut output),
        Err(Error::StaleToken)
    );
    let fresh = CandidatePage::new(new_session, &["你", "呢"], Some(1)).unwrap();
    let commit = state.commit_candidate(&fresh, 1, &mut output).unwrap();
    assert_eq!(&output[..commit.bytes_written], "呢".as_bytes());
}

#[test]
fn empty_buffers_and_invalid_candidates_have_explicit_results() {
    assert_eq!(ContextId::new(0), Err(Error::InvalidContext));
    let mut storage = [];
    let mut state = Composition::new(&mut storage);
    state.set_focus(Some(ContextId::new(1).unwrap()));
    let token = state.begin().unwrap();
    assert_eq!(state.begin(), Err(Error::AlreadyComposing));
    let token = state.set_preedit(token, "", Selection::caret(0)).unwrap();
    assert_eq!(
        state.set_preedit(token, "a", Selection::caret(1)),
        Err(Error::BufferTooSmall)
    );
    assert!(CandidatePage::new(token, &[], Some(0)).is_err());
    let page = CandidatePage::new(token, &[], None).unwrap();
    assert!(page.is_empty());
    assert_eq!(page.get(0), Err(Error::InvalidCandidate));
    let mut output = [];
    assert_eq!(
        state
            .commit_preedit(token, &mut output)
            .unwrap()
            .bytes_written,
        0
    );
}

#[test]
fn blur_cancel_and_drop_erase_retained_preedit() {
    for operation in 0..3 {
        let mut storage = [0; 32];
        {
            let mut state = Composition::new(&mut storage);
            state.set_focus(Some(ContextId::new(1).unwrap()));
            let token = state.begin().unwrap();
            let token = state
                .set_preedit(token, "private text", Selection::caret(12))
                .unwrap();
            let token = state.set_preedit(token, "x", Selection::caret(1)).unwrap();
            match operation {
                0 => {
                    state.set_focus(None);
                }
                1 => state.cancel(token).unwrap(),
                _ => {}
            }
        }
        assert_eq!(storage, [0; 32]);
    }
}

#[test]
fn emitted_text_uses_explicit_context_and_caller_owned_commit() {
    struct Sink {
        context: Option<ContextId>,
        text: String,
    }
    impl TextTarget for Sink {
        type Error = core::convert::Infallible;
        fn commit_text(&mut self, context: ContextId, text: &str) -> Result<(), Self::Error> {
            self.context = Some(context);
            self.text.push_str(text);
            Ok(())
        }
    }
    let mut storage = [0; 32];
    let mut output = [0; 32];
    let context = ContextId::new(42).unwrap();
    let mut state = Composition::new(&mut storage);
    state.set_focus(Some(context));
    let token = state.begin().unwrap();
    let commit = state.commit(token, "你好", &mut output).unwrap();
    let mut sink = Sink {
        context: None,
        text: String::new(),
    };
    sink.commit_text(
        commit.context,
        core::str::from_utf8(&output[..commit.bytes_written]).unwrap(),
    )
    .unwrap();
    assert_eq!(sink.context, Some(context));
    assert_eq!(sink.text, "你好");
}
