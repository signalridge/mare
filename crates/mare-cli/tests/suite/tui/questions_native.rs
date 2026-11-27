//! Multi-select questions preserve choices while presenting custom input.

use super::*;

/// Multiple selected values can be combined with a custom answer (reference implementation
/// `interaction/tool-ask-user/tests/tool-ask-user.spec.ts:364`).
#[tokio::test]
async fn multi_question_confirms_choices_and_custom_answer() {
    local(async {
        let mut t = Term::new(json!([]), 120, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"mare.task.spawn(function()
                 ANSWERS = mare.ui.questions({ questions = { { question_id = 'targets', question = 'Pick targets',
                   multi_select = true, options = { { label = 'tests' }, { label = 'docs' } } } } })
               end)"#,
        )
        .await;
        t.until("question", |s| s.contains("Pick targets") && s.contains("Other")).await;
        assert!(!t.screen().contains("Done (confirm selection)"), "{}", t.screen());
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.key(KeyCode::Down, Modifiers::NONE);
        t.key(KeyCode::Down, Modifiers::NONE);
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("custom answer", |s| s.contains("Enter custom answer")).await;
        t.vt.type_str("release notes");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("question closed", |s| !s.contains("Enter custom answer")).await;
        assert!(eval::<bool>(&t.vt.host, "return ANSWERS.answers.targets.selected[1] == 'tests' and ANSWERS.answers.targets.custom == 'release notes'").await);
    })
    .await
}

#[tokio::test]
async fn question_can_disable_custom_and_skip() {
    local(async {
        let mut t = Term::new(json!([]), 100, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"mare.task.spawn(function()
                 ANSWERS = mare.ui.questions({ questions = { { question_id = 'targets', question = 'Required choices',
                   multi_select = true, allow_custom = false, allow_skip = false,
                   options = { { label = 'one' }, { label = 'two' } } },
                   { question_id = 'single', question = 'Required single', allow_custom = false, allow_skip = false,
                     options = { { label = 'only choice' } } } } })
               end)"#,
        )
        .await;
        t.until("required question", |s| s.contains("Required choices") && s.contains("one")).await;
        let screen = t.screen();
        assert!(!screen.contains("Other") && !screen.contains("Skip") && !screen.contains("Done (confirm"), "{screen}");
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("required single question", |s| s.contains("Required single") && s.contains("only choice")).await;
        let screen = t.screen();
        assert!(!screen.contains("Other") && !screen.contains("Skip"), "{screen}");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("required question closed", |s| !s.contains("Required single")).await;
        assert!(eval::<bool>(&t.vt.host, "return ANSWERS.answers.targets.selected[1] == 'one' and ANSWERS.answers.single.selected[1] == 'only choice'").await);
    })
    .await
}
