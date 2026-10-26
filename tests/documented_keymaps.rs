//! Contract tests for the documented keymap.
//!
//! `AGENTS.md` and `docs/keymaps.md` record the editing contract. These tests keep
//! those promises, `--help`, and the editor itself from drifting apart: the key
//! inventory and Ex command list are pinned, and every example printed in the
//! keymap page is executed against the editor.

use std::collections::BTreeSet;

use postvim::cli::HELP;
use postvim::{Document, Editor, EditorInput, EditorMode};

const KEYMAP: &str = include_str!("../docs/keymaps.md");
const AGENTS: &str = include_str!("../AGENTS.md");

/// The dirty-quit sentence the agent guide and keymap page both promise.
const DOCUMENTED_DIRTY_MESSAGE: &str =
    "Unsaved changes; use :w [path] before :q, or :q! to discard";

/// Every key the documentation may claim, as a whitespace-separated inventory.
const REQUIRED_KEYS: &str = "\
    {count} h j k l w W b B e E 0 ^ $ gg G f F t T ; , i I a A o O x X r s S D C \
    d c y dd cc yy p P u Ctrl-r . v V / ? n N : Enter Tab Backspace Delete Left Right \
    Up Down Esc";

/// The complete Ex grammar; only `:w` takes an argument.
const REQUIRED_EX_COMMANDS: [&str; 6] = [":w", ":w PATH", ":q", ":q!", ":wq", ":x"];

/// Keys and commands the documentation must call out as unavailable.
const REQUIRED_UNSUPPORTED: &str = "\
    di ci yi Ctrl-v \" @ q m R Y J ~ ZZ ZQ gv :s :! :1,$d :wq<path> :x<path> :w! :q!!";

/// One executable example from the keymap page.
struct Scenario {
    start: String,
    keys: Vec<EditorInput>,
    expected: String,
    mode: EditorMode,
    /// Row and UTF-8 byte offset of the grapheme the cursor should rest on.
    cursor: (usize, usize),
}

fn block_lines(document: &str, name: &str) -> Vec<String> {
    let marker = format!("```text {name}");
    let mut lines = Vec::new();
    let mut collecting = false;
    for line in document.lines() {
        let trimmed = line.trim();
        if !collecting {
            collecting = trimmed == marker;
            continue;
        }
        if trimmed == "```" {
            return lines;
        }
        if !trimmed.is_empty() {
            lines.push(trimmed.to_owned());
        }
    }
    panic!("docs/keymaps.md has no terminated {name:?} block");
}

/// The key column of an inventory entry, for example `h j k l`.
fn entry_keys(entry: &str) -> Vec<String> {
    let keys = entry.split('|').next().unwrap_or_default();
    keys.split_whitespace().map(str::to_owned).collect()
}

fn keys_in_block(document: &str, name: &str) -> BTreeSet<String> {
    block_lines(document, name)
        .iter()
        .flat_map(|entry| entry_keys(entry))
        .collect()
}

fn required<const N: usize>(items: [&str; N]) -> BTreeSet<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn words(spec: &str) -> BTreeSet<String> {
    spec.split_whitespace().map(str::to_owned).collect()
}

fn quoted(text: &str) -> String {
    let inner = text
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or_else(|| panic!("expected a quoted buffer, found {text:?}"));
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            other => panic!("unsupported escape in {text:?}: {other:?}"),
        }
    }
    out
}

fn key_inputs(keys: &str) -> Vec<EditorInput> {
    let mut inputs = Vec::new();
    let mut rest = keys;
    while let Some(ch) = rest.chars().next() {
        if ch != '<' {
            inputs.push(EditorInput::Character(ch));
            rest = &rest[ch.len_utf8()..];
            continue;
        }
        let end = rest
            .find('>')
            .unwrap_or_else(|| panic!("unterminated key token in {keys:?}"));
        inputs.push(match &rest[1..end] {
            "Esc" => EditorInput::Escape,
            "CR" => EditorInput::Enter,
            "Tab" => EditorInput::Tab,
            "BS" => EditorInput::Backspace,
            other => panic!("unknown key token <{other}>"),
        });
        rest = &rest[end + 1..];
    }
    inputs
}

fn documented_mode(name: &str) -> EditorMode {
    match name {
        "normal" => EditorMode::Normal,
        other => panic!("documented examples must end in normal mode, found {other:?}"),
    }
}

fn documented_cursor(text: &str) -> (usize, usize) {
    let (row, byte) = text
        .split_once(':')
        .unwrap_or_else(|| panic!("expected a row:byte cursor, found {text:?}"));
    (
        row.trim().parse().expect("cursor row"),
        byte.trim().parse().expect("cursor byte offset"),
    )
}

fn scenarios() -> Vec<Scenario> {
    block_lines(KEYMAP, "postvim-examples")
        .iter()
        .map(|line| {
            let fields: Vec<&str> = line.split(" | ").map(str::trim).collect();
            assert_eq!(fields.len(), 5, "malformed documented example: {line:?}");
            Scenario {
                start: quoted(fields[0]),
                keys: key_inputs(fields[1]),
                expected: quoted(fields[2]),
                mode: documented_mode(fields[3]),
                cursor: documented_cursor(fields[4]),
            }
        })
        .collect()
}

fn key_label(input: &EditorInput) -> Option<String> {
    match input {
        EditorInput::Character(ch) => Some(ch.to_string()),
        EditorInput::Enter => Some("Enter".to_owned()),
        EditorInput::Tab => Some("Tab".to_owned()),
        EditorInput::Backspace => Some("Backspace".to_owned()),
        EditorInput::Escape => Some("Esc".to_owned()),
        _ => None,
    }
}

#[test]
fn documented_keymap_scenarios() {
    let inventory = keys_in_block(KEYMAP, "postvim-inventory");
    assert_eq!(
        inventory,
        words(REQUIRED_KEYS),
        "docs/keymaps.md must document exactly the implemented key inventory"
    );

    let commands: BTreeSet<String> = block_lines(KEYMAP, "postvim-ex-commands")
        .into_iter()
        .collect();
    assert_eq!(
        commands,
        required(REQUIRED_EX_COMMANDS),
        "docs/keymaps.md must document exactly the supported Ex commands"
    );

    let unsupported = keys_in_block(KEYMAP, "postvim-unsupported");
    for key in REQUIRED_UNSUPPORTED.split_whitespace() {
        assert!(
            unsupported.contains(key),
            "docs/keymaps.md must list {key:?} as unsupported"
        );
    }
    let contradictory: Vec<&String> = inventory.intersection(&unsupported).collect();
    assert!(
        contradictory.is_empty(),
        "keys cannot be documented as both supported and unsupported: {contradictory:?}"
    );

    let examples = scenarios();
    assert!(
        examples.len() >= 8,
        "expected the documented examples, found {}",
        examples.len()
    );
    // Examples may spell out literal Insert text and may use an explicitly
    // unsupported key to show that it cancels, but every key an example relies
    // on has to be described on the page: supported, unsupported, or a count digit.
    let documented: BTreeSet<String> = inventory
        .iter()
        .chain(unsupported.iter())
        .cloned()
        .collect();
    for (index, scenario) in examples.iter().enumerate() {
        for input in &scenario.keys {
            let token = key_label(input).unwrap_or_else(|| {
                panic!("example {index} uses an input the page cannot describe: {input:?}")
            });
            let counted = !token.is_empty() && token.chars().all(|ch| ch.is_ascii_digit());
            assert!(
                counted || documented.contains(&token),
                "example {index} uses the undocumented key {token:?}"
            );
        }
    }
}

#[test]
fn documented_editing_examples() {
    let mut failures = Vec::new();
    for (index, scenario) in scenarios().iter().enumerate() {
        let mut editor = Editor::from_document(Document::from_text(&scenario.start));
        for input in &scenario.keys {
            editor.handle_input(input.clone());
        }
        let keystrokes: String = scenario
            .keys
            .iter()
            .filter_map(key_label)
            .collect::<Vec<_>>()
            .join("");
        let mut problems = Vec::new();
        let text = editor.document().serialize();
        if text != scenario.expected {
            problems.push(format!(
                "buffer is {text:?}, documented {:?}",
                scenario.expected
            ));
        }
        if editor.mode() != scenario.mode {
            problems.push(format!(
                "mode is {:?}, documented {:?}",
                editor.mode(),
                scenario.mode
            ));
        }
        let cursor = editor.cursor();
        if (cursor.row, cursor.column) != scenario.cursor {
            problems.push(format!(
                "cursor is {:?}:{}, documented {:?}:{}",
                cursor.row, cursor.column, scenario.cursor.0, scenario.cursor.1
            ));
        }
        if !problems.is_empty() {
            failures.push(format!(
                "example {index} {keystrokes:?} from {:?}: {}",
                scenario.start,
                problems.join("; ")
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "docs/keymaps.md examples do not match the editor:\n{}",
        failures.join("\n")
    );
}

#[test]
fn help_matches_save_exit_contract() {
    for command in REQUIRED_EX_COMMANDS {
        assert!(HELP.contains(command), "--help must document {command:?}");
    }
    for command in [":w", ":q", ":q!", ":wq", ":x"] {
        assert!(
            AGENTS.contains(command),
            "AGENTS.md must document {command:?}"
        );
    }
    assert!(
        AGENTS.contains(":w <path>"),
        "AGENTS.md must document the path form of :w"
    );
    for claim in ["discards", "modified"] {
        assert!(
            HELP.contains(claim),
            "--help must state that :q! discards and :x writes only when modified"
        );
    }
    for stale in ["There is no force-quit", "There is no undo"] {
        assert!(!HELP.contains(stale), "--help still claims {stale:?}");
    }
    assert!(
        AGENTS.contains(DOCUMENTED_DIRTY_MESSAGE),
        "AGENTS.md must state the dirty-quit message exactly"
    );
    assert!(
        KEYMAP.contains(DOCUMENTED_DIRTY_MESSAGE),
        "docs/keymaps.md must state the dirty-quit message exactly"
    );
    assert!(
        AGENTS.contains("docs/keymaps.md"),
        "AGENTS.md must point at the keymap contract"
    );
}
