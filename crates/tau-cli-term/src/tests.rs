use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;

fn new_test_term_with_data(
    commands: Vec<SlashCommand>,
) -> (
    HighTerm,
    TermHandle,
    CompletionData,
    std::sync::mpsc::Sender<TestRawEvent>,
) {
    let (raw_term, handle, input_tx) = tau_cli_term_raw::Term::new_virtual(
        80,
        24,
        "> ",
        Box::new(std::io::sink()),
        CursorShape::Bar,
    );
    let (term, completion_data) =
        HighTerm::new_for_test(raw_term, handle.clone(), commands, Theme::builtin());
    (term, handle, completion_data, input_tx)
}

fn new_test_term(
    commands: Vec<SlashCommand>,
) -> (HighTerm, TermHandle, std::sync::mpsc::Sender<TestRawEvent>) {
    let (term, handle, _completion_data, input_tx) = new_test_term_with_data(commands);
    (term, handle, input_tx)
}

fn send_key(input_tx: &std::sync::mpsc::Sender<TestRawEvent>, code: KeyCode) {
    input_tx
        .send(TestRawEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .expect("send key");
}

fn submit(
    term: &mut HighTerm,
    handle: &TermHandle,
    input_tx: &std::sync::mpsc::Sender<TestRawEvent>,
    line: &str,
) {
    handle.set_buffer(line.to_owned(), line.len());
    send_key(input_tx, KeyCode::Enter);
    assert!(matches!(
        term.get_next_event().expect("submit line"),
        Event::Line(submitted) if submitted == line
    ));
}

fn type_text(term: &mut HighTerm, input_tx: &std::sync::mpsc::Sender<TestRawEvent>, text: &str) {
    for ch in text.chars() {
        send_key(input_tx, KeyCode::Char(ch));
        assert!(matches!(
            term.get_next_event().expect("type char"),
            Event::BufferChanged
        ));
    }
}

fn submit_typed(term: &mut HighTerm, input_tx: &std::sync::mpsc::Sender<TestRawEvent>, line: &str) {
    type_text(term, input_tx, line);
    send_key(input_tx, KeyCode::Enter);
    assert!(matches!(
        term.get_next_event().expect("submit typed line"),
        Event::Line(submitted) if submitted == line
    ));
}

#[test]
fn typed_history_item_matching_completion_needs_one_up_per_item() {
    let (mut term, handle, input_tx) = new_test_term(vec![
        SlashCommand::new("/model", "Switch model"),
        SlashCommand::new("/quit", "Exit"),
    ]);

    submit_typed(&mut term, &input_tx, "Hi");
    submit_typed(&mut term, &input_tx, "/model openai/gpt-5");

    send_key(&input_tx, KeyCode::Up);
    assert!(matches!(
        term.get_next_event()
            .expect("navigate to slash history item"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/model openai/gpt-5");

    send_key(&input_tx, KeyCode::Up);
    assert!(matches!(
        term.get_next_event().expect("continue history navigation"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "Hi");
}

#[test]
fn history_after_accepting_argument_completion_needs_one_up_per_item() {
    let (mut term, handle, completion_data, input_tx) = new_test_term_with_data(vec![
        SlashCommand::new("/model", "Switch model"),
        SlashCommand::new("/quit", "Exit"),
    ]);
    completion_data.set_arg_completions(
        CommandName::new("/model"),
        vec![CompletionItem::plain("openai/gpt-5")],
    );

    submit_typed(&mut term, &input_tx, "Hi");
    type_text(&mut term, &input_tx, "/model op");

    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event().expect("cycle argument completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/model openai/gpt-5");

    send_key(&input_tx, KeyCode::Enter);
    send_key(&input_tx, KeyCode::Enter);
    assert!(matches!(
        term.get_next_event().expect("accept and submit completion"),
        Event::Line(line) if line == "/model openai/gpt-5"
    ));

    send_key(&input_tx, KeyCode::Up);
    assert!(matches!(
        term.get_next_event()
            .expect("navigate to completed history item"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/model openai/gpt-5");

    send_key(&input_tx, KeyCode::Up);
    assert!(matches!(
        term.get_next_event().expect("continue history navigation"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "Hi");
}

#[test]
fn history_items_matching_completion_do_not_steal_following_history_navigation() {
    let (mut term, handle, input_tx) = new_test_term(vec![
        SlashCommand::new("/model", "Switch model"),
        SlashCommand::new("/quit", "Exit"),
    ]);

    submit(&mut term, &handle, &input_tx, "Hi");
    submit(&mut term, &handle, &input_tx, "/model openai/gpt-5");

    send_key(&input_tx, KeyCode::Up);
    assert!(matches!(
        term.get_next_event()
            .expect("navigate to slash history item"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/model openai/gpt-5");

    send_key(&input_tx, KeyCode::Up);
    assert!(matches!(
        term.get_next_event().expect("continue history navigation"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "Hi");
}

#[test]
fn up_arrow_cycles_completion_after_down_cycles_with_history_present() {
    let (mut term, handle, completion_data, input_tx) = new_test_term_with_data(vec![
        SlashCommand::new("/model", "Switch model"),
        SlashCommand::new("/quit", "Exit"),
    ]);
    completion_data.set_arg_completions(
        CommandName::new("/model"),
        vec![
            CompletionItem::plain("anthropic/claude-sonnet-4-5"),
            CompletionItem::plain("openai/gpt-5"),
            CompletionItem::plain("openai/gpt-5-mini"),
        ],
    );

    submit_typed(&mut term, &input_tx, "Hi");
    type_text(&mut term, &input_tx, "/model ");

    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event().expect("cycle to first model"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/model anthropic/claude-sonnet-4-5");

    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event().expect("cycle to second model"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/model openai/gpt-5");

    send_key(&input_tx, KeyCode::Up);
    assert!(matches!(
        term.get_next_event().expect("cycle back to first model"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/model anthropic/claude-sonnet-4-5");
}

#[test]
fn arrows_cycle_active_completion_even_when_history_exists() {
    let (mut term, handle, input_tx) = new_test_term(vec![
        SlashCommand::new("/model", "Switch model"),
        SlashCommand::new("/quit", "Exit"),
    ]);

    submit(&mut term, &handle, &input_tx, "Hi");

    send_key(&input_tx, KeyCode::Char('/'));
    assert!(matches!(
        term.get_next_event().expect("trigger completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/");

    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event()
            .expect("cycle completion with history present"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/model");

    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event()
            .expect("cycle completion again with history present"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/quit");
}

#[test]
fn up_at_first_match_returns_to_original_buffer_then_wraps() {
    // From idx 0, Up returns to the un-selected state (no preview),
    // restoring the original buffer the user typed. A *second* Up
    // wraps around to the last candidate. This is the symmetric,
    // four-state cycle: None → 0 → 1 → ... → len-1 → 0 → 1 → ...,
    // with one None reachable on the Up-from-0 boundary.
    let (mut term, handle, input_tx) = new_test_term(vec![
        SlashCommand::new("/model", "Switch model"),
        SlashCommand::new("/quit", "Exit"),
    ]);

    send_key(&input_tx, KeyCode::Char('/'));
    assert!(matches!(
        term.get_next_event().expect("trigger completion"),
        Event::BufferChanged
    ));

    let sequence: &[(KeyCode, &str)] = &[
        (KeyCode::Down, "/model"),
        (KeyCode::Down, "/quit"),
        (KeyCode::Up, "/model"),
        // Up from idx 0 → no selection → buffer is restored to what
        // the user actually typed.
        (KeyCode::Up, "/"),
        // Continuing Up from None wraps to the last match.
        (KeyCode::Up, "/quit"),
    ];
    for (i, (key, want)) in sequence.iter().enumerate() {
        send_key(&input_tx, *key);
        assert!(matches!(
            term.get_next_event().expect("cycle"),
            Event::BufferChanged
        ));
        assert_eq!(
            handle.get_buffer(),
            *want,
            "step {} ({key:?}): expected {want:?}, got {:?}",
            i + 1,
            handle.get_buffer()
        );
    }
}

#[test]
fn arrows_cycle_repeatedly_through_completion_with_history_present() {
    // With prior submitted lines, Down at the prompt would normally
    // route to history navigation. The mode-driven dispatch in raw
    // gives the open completion menu first claim on Up/Down, so the
    // arrows cycle the menu and the history is never touched.
    let (mut term, handle, input_tx) = new_test_term(vec![
        SlashCommand::new("/model", "Switch model"),
        SlashCommand::new("/quit", "Exit"),
    ]);

    submit(&mut term, &handle, &input_tx, "earlier-1");
    submit(&mut term, &handle, &input_tx, "earlier-2");

    send_key(&input_tx, KeyCode::Char('/'));
    assert!(matches!(
        term.get_next_event().expect("trigger completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/");

    let expected = ["/model", "/quit", "/model", "/quit"];
    for (i, want) in expected.iter().enumerate() {
        send_key(&input_tx, KeyCode::Down);
        assert!(matches!(
            term.get_next_event().expect("cycle completion"),
            Event::BufferChanged
        ));
        assert_eq!(
            handle.get_buffer(),
            *want,
            "after {} Down keypresses (with history present) the buffer \
             should be {want:?}, got {:?}",
            i + 1,
            handle.get_buffer()
        );
    }
}

#[test]
fn arrows_cycle_repeatedly_through_completion_suggestions() {
    // Down four times should cycle: /model, /quit, /model, /quit.
    // Wrapping is the normal `(i + 1) mod len` — the None state is
    // only reachable via Up at idx 0.
    let (mut term, handle, input_tx) = new_test_term(vec![
        SlashCommand::new("/model", "Switch model"),
        SlashCommand::new("/quit", "Exit"),
    ]);

    send_key(&input_tx, KeyCode::Char('/'));
    assert!(matches!(
        term.get_next_event().expect("trigger completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/");

    let expected = ["/model", "/quit", "/model", "/quit"];
    for (i, want) in expected.iter().enumerate() {
        send_key(&input_tx, KeyCode::Down);
        assert!(matches!(
            term.get_next_event().expect("cycle completion"),
            Event::BufferChanged
        ));
        assert_eq!(
            handle.get_buffer(),
            *want,
            "after {} Down keypresses the buffer should be {want:?}, got {:?}",
            i + 1,
            handle.get_buffer()
        );
    }
}

#[test]
fn arrows_still_cycle_active_completion_suggestions() {
    let (mut term, handle, input_tx) = new_test_term(vec![
        SlashCommand::new("/model", "Switch model"),
        SlashCommand::new("/quit", "Exit"),
    ]);

    send_key(&input_tx, KeyCode::Char('/'));
    assert!(matches!(
        term.get_next_event().expect("trigger completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/");

    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event().expect("cycle completion"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/model");

    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event().expect("cycle completion again"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/quit");
}

#[test]
fn editing_after_preview_commits_it_as_the_new_original_buffer() {
    // Once the user has cycled to a candidate and started editing
    // the previewed text, Esc should drop them back at *the edited
    // preview*, not at the prefix they originally typed before
    // opening the menu. This pins the "every edit commits the prior
    // preview" rule the raw layer documents in `refresh_completion`.
    let (mut term, handle, input_tx) = new_test_term(vec![
        SlashCommand::new("/model", "Switch model"),
        SlashCommand::new("/quit", "Exit"),
    ]);

    type_text(&mut term, &input_tx, "/m");
    assert_eq!(handle.get_buffer(), "/m");

    // Cycle to "/model" — buffer now previews the candidate.
    send_key(&input_tx, KeyCode::Down);
    assert!(matches!(
        term.get_next_event().expect("preview /model"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/model");

    // Backspace edits the preview. The new buffer ("/mode") still
    // matches "/model" by prefix, so the menu re-opens — but with
    // "/mode" as the new original.
    send_key(&input_tx, KeyCode::Backspace);
    assert!(matches!(
        term.get_next_event().expect("backspace edits preview"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/mode");

    // Esc dismisses to the edited preview, not back to "/m".
    send_key(&input_tx, KeyCode::Esc);
    assert!(matches!(
        term.get_next_event()
            .expect("esc returns to edited preview"),
        Event::BufferChanged
    ));
    assert_eq!(handle.get_buffer(), "/mode");
}
