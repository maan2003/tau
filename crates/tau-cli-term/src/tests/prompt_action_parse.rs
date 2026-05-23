use crate::PromptShellAction;

#[test]
fn parses_history_actions() {
    assert!(matches!(
        PromptShellAction::parse("prompt-next"),
        Some(PromptShellAction::PromptNext)
    ));
    assert!(matches!(
        PromptShellAction::parse("prompt-previous"),
        Some(PromptShellAction::PromptPrevious)
    ));
    assert!(matches!(
        PromptShellAction::parse("prompt-undo"),
        Some(PromptShellAction::PromptUndo)
    ));
    assert!(matches!(
        PromptShellAction::parse("prompt-redo"),
        Some(PromptShellAction::PromptRedo)
    ));
}

#[test]
fn parses_prompt_submit_and_newline_actions() {
    assert!(matches!(
        PromptShellAction::parse("submit-prompt"),
        Some(PromptShellAction::SubmitPrompt)
    ));
    assert!(matches!(
        PromptShellAction::parse("insert-newline"),
        Some(PromptShellAction::InsertNewline)
    ));
}

#[test]
fn parses_shell_insert_with_trim() {
    let parsed = PromptShellAction::parse("shell-prompt-insert:trim:echo hi");
    match parsed {
        Some(PromptShellAction::Insert(cmd)) => {
            assert!(cmd.trim);
            assert_eq!(cmd.command, "echo hi");
        }
        _ => panic!("expected Insert"),
    }
}

#[test]
fn parses_shell_edit_preserves_colons_in_command() {
    let parsed = PromptShellAction::parse("shell-prompt-edit:full:bash -c 'echo a:b:c'");
    match parsed {
        Some(PromptShellAction::Edit(cmd)) => {
            assert!(!cmd.trim);
            assert_eq!(cmd.command, "bash -c 'echo a:b:c'");
        }
        _ => panic!("expected Edit"),
    }
}

#[test]
fn parses_prompt_history_search_with_trim() {
    let parsed = PromptShellAction::parse("prompt-history-search:trim:fzf | cut -f1");
    match parsed {
        Some(PromptShellAction::HistorySearch(cmd)) => {
            assert!(cmd.trim);
            assert_eq!(cmd.command, "fzf | cut -f1");
        }
        _ => panic!("expected HistorySearch"),
    }
}

#[test]
fn parses_fast_toggle() {
    assert!(matches!(
        PromptShellAction::parse("fast-toggle"),
        Some(PromptShellAction::FastToggle)
    ));
}

#[test]
fn parses_role_cycle() {
    assert!(matches!(
        PromptShellAction::parse("cycle-role"),
        Some(PromptShellAction::CycleRole)
    ));
    assert!(matches!(
        PromptShellAction::parse("cycle-role-group"),
        Some(PromptShellAction::CycleRoleGroup)
    ));
}

#[test]
fn unknown_action_returns_none() {
    assert!(PromptShellAction::parse("not-a-real-action").is_none());
    assert!(PromptShellAction::parse("shell-prompt-bogus:trim:cmd").is_none());
    assert!(PromptShellAction::parse("shell-prompt-edit:trim").is_none());
}
