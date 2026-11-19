#![cfg(feature = "plugins")]

use std::{env, ffi::CString};
use zinput::engine::zplugin::{Error, Plugin};
use zinput::engine::{InputEngineApi, change, key, phase};

#[test]
#[ignore = "build example ime_engine and set ZINPUT_TEST_PLUGIN to its native library"]
fn separately_compiled_engine_preserves_state_across_abi_errors() {
    let path = env::var("ZINPUT_TEST_PLUGIN").expect("ZINPUT_TEST_PLUGIN is required");
    let path = CString::new(path).expect("plugin path has no embedded NUL");
    // SAFETY: The test loads this repository's reviewed reference engine. The
    // pinned policy keeps its code mapped while all interface objects are used.
    let plugin = unsafe { Plugin::load(&path) }.unwrap();
    let mut engine = InputEngineApi::create(&plugin).unwrap();
    engine.reset(41).unwrap();
    assert_eq!(
        engine.process(99, key::TEXT, 0, "x"),
        Err(Error::InvalidArgument)
    );
    assert_eq!(
        engine.process(phase::PRESS, key::TEXT, 0, "ni").unwrap(),
        change::HANDLED | change::PREEDIT | change::CANDIDATES
    );
    assert_eq!(engine.candidate_count().unwrap(), 2);
    let mut output = [0xa5; 256];
    assert_eq!(engine.preedit(&mut output).unwrap(), 2);
    assert_eq!(&output[..2], b"ni");
    assert_eq!(engine.selection().unwrap().start, 2);
    assert_eq!(engine.selection().unwrap().end, 2);
    assert_eq!(engine.candidate(1, &mut output).unwrap(), "呢".len() as u64);
    assert_eq!(&output[.."呢".len()], "呢".as_bytes());

    let mut short = [0xa5; 2];
    assert_eq!(engine.candidate(0, &mut short), Err(Error::BufferTooSmall));
    assert_eq!(short, [0xa5; 2]);
    assert_eq!(engine.select(2), Err(Error::InvalidArgument));
    assert_eq!(engine.candidate_count().unwrap(), 2);
    assert_ne!(engine.select(1).unwrap() & change::COMMIT, 0);
    assert_eq!(engine.commit(&mut short), Err(Error::BufferTooSmall));
    assert_eq!(short, [0xa5; 2]);
    assert_eq!(
        engine.process(phase::PRESS, key::TEXT, 0, "lost"),
        Err(Error::Busy)
    );
    assert_eq!(engine.commit(&mut output).unwrap(), 3);
    assert_eq!(&output[..3], "呢".as_bytes());
    assert_eq!(engine.commit(&mut output).unwrap(), 0);

    for text in ["日本語", "한글", "العربية", "हिन्दी", "🙂é"] {
        engine.process(phase::PRESS, key::TEXT, 0, text).unwrap();
        engine
            .process(phase::RELEASE, key::TEXT, 0, "ignored")
            .unwrap();
        engine.process(phase::PRESS, key::ENTER, 0, "").unwrap();
        let len = engine.commit(&mut output).unwrap() as usize;
        assert_eq!(&output[..len], text.as_bytes());
    }

    engine.process(phase::PRESS, key::TEXT, 0, "🙂").unwrap();
    assert_eq!(
        engine.process(phase::PRESS, key::TEXT, 0, &"a".repeat(128)),
        Err(Error::BufferTooSmall)
    );
    assert_eq!(engine.preedit(&mut output).unwrap(), 4);
    assert_eq!(&output[..4], "🙂".as_bytes());
    engine.process(phase::PRESS, key::BACKSPACE, 0, "").unwrap();
    assert_eq!(engine.preedit(&mut output).unwrap(), 0);

    engine
        .process(phase::PRESS, key::TEXT, 0, "old context")
        .unwrap();
    engine.process(phase::PRESS, key::ENTER, 0, "").unwrap();
    engine.reset(42).unwrap();
    assert_eq!(engine.commit(&mut output).unwrap(), 0);
    assert_eq!(engine.preedit(&mut output).unwrap(), 0);
    engine.reset(0).unwrap();
    assert_eq!(
        engine.process(phase::PRESS, key::TEXT, 0, "x"),
        Err(Error::InvalidArgument)
    );
}
