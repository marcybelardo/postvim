//! Pending-input parser for Normal and Visual mode keystrokes.
//!
//! The parser owns only the typing state a command needs before it can execute:
//! an optional leading count, a pending `g`, an operator with its own optional
//! motion count, and an awaited `f`/`F`/`t`/`T` target or `r` replacement.
//! Resolving motions and editing the document stay with the editor.

use crate::motion::{FindKind, MotionSpec};

/// Counts saturate here so no traversal or allocation is driven by unchecked input.
pub const COUNT_LIMIT: usize = 1_000_000;

pub fn append_digit(count: usize, ch: char) -> usize {
    let digit = ch.to_digit(10).unwrap_or(0) as usize;
    count
        .saturating_mul(10)
        .saturating_add(digit)
        .min(COUNT_LIMIT)
}

pub fn multiply(first: Option<usize>, second: Option<usize>) -> usize {
    first
        .unwrap_or(1)
        .saturating_mul(second.unwrap_or(1))
        .min(COUNT_LIMIT)
}

/// Digits of an explicitly supplied count, including `1`.
///
/// Callers pass digits only when a count was actually typed, so a recipe can
/// distinguish `1dG` from `dG` on replay.
pub fn count_digits(count: usize) -> Vec<char> {
    count.to_string().chars().collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operator {
    Delete,
    Change,
    Yank,
}

impl Operator {
    pub fn key(self) -> char {
        match self {
            Self::Delete => 'd',
            Self::Change => 'c',
            Self::Yank => 'y',
        }
    }

    pub fn from_key(ch: char) -> Option<Self> {
        match ch {
            'd' => Some(Self::Delete),
            'c' => Some(Self::Change),
            'y' => Some(Self::Yank),
            _ => None,
        }
    }
}

/// How an Insert session starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InsertEntry {
    /// `i`
    Before,
    /// `I`
    FirstNonBlank,
    /// `a`
    After,
    /// `A`
    LineEnd,
    /// `o`
    OpenBelow,
    /// `O`
    OpenAbove,
    /// `c`, `s`, `C`, or a Visual change: the cursor already sits at the
    /// insertion boundary left by the change.
    Change,
}

impl InsertEntry {
    pub fn key(self) -> char {
        match self {
            Self::Before => 'i',
            Self::FirstNonBlank => 'I',
            Self::After => 'a',
            Self::LineEnd => 'A',
            Self::OpenBelow => 'o',
            Self::OpenAbove => 'O',
            Self::Change => 'c',
        }
    }

    pub fn from_key(ch: char) -> Option<Self> {
        match ch {
            'i' => Some(Self::Before),
            'I' => Some(Self::FirstNonBlank),
            'a' => Some(Self::After),
            'A' => Some(Self::LineEnd),
            'o' => Some(Self::OpenBelow),
            'O' => Some(Self::OpenAbove),
            _ => None,
        }
    }

    pub fn is_open(self) -> bool {
        matches!(self, Self::OpenBelow | Self::OpenAbove)
    }
}

/// Target character for `r`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplaceTarget {
    Char(char),
    Tab,
    LineBreak,
}

/// What the editor should do with one keystroke.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Step {
    /// Pending state changed; nothing executes yet.
    Waiting,
    /// The prefix was abandoned; nothing executes.
    Cancel,
    /// A complete non-motion command such as `x`, `p`, or `i`.
    Command { key: char, count: Option<usize> },
    Motion {
        spec: MotionSpec,
        count: Option<usize>,
    },
    Operator {
        op: Operator,
        spec: MotionSpec,
        count: Option<usize>,
    },
    /// `dd`, `cc`, `yy`, and count-collapsed forms such as `2y3y`.
    OperatorLinewise { op: Operator, count: usize },
    OperatorFind {
        op: Operator,
        kind: FindKind,
        target: char,
        count: usize,
    },
    Find {
        kind: FindKind,
        target: char,
        count: usize,
    },
    Replace {
        target: ReplaceTarget,
        /// `Some` only when a count was typed; `1rX` and `rX` are equivalent but
        /// the presence is kept so repeat recipes stay exact.
        count: Option<usize>,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum State {
    #[default]
    Idle,
    Count(usize),
    Goto(Option<usize>),
    Operator {
        op: Operator,
        count1: Option<usize>,
    },
    OperatorCount {
        op: Operator,
        count1: Option<usize>,
        count2: usize,
    },
    OperatorGoto {
        op: Operator,
        count1: Option<usize>,
    },
    OperatorFind {
        op: Operator,
        count1: Option<usize>,
        kind: FindKind,
    },
    Find {
        kind: FindKind,
        count: usize,
    },
    Replace {
        count: Option<usize>,
    },
}

/// One keystroke of pending command state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Parser {
    state: State,
}

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.state = State::Idle;
    }

    pub fn is_idle(&self) -> bool {
        matches!(self.state, State::Idle)
    }

    /// Consumes and returns a pending leading count, if any.
    pub fn take_count(&mut self) -> Option<usize> {
        match self.state {
            State::Count(count) => {
                self.state = State::Idle;
                Some(count)
            }
            _ => None,
        }
    }

    /// Compact status text for the pending prefix, e.g. `2d`, `df`, or `2`.
    pub fn display(&self) -> String {
        fn prefix(count: Option<usize>) -> String {
            count.map(|count| count.to_string()).unwrap_or_default()
        }
        match self.state {
            State::Idle => String::new(),
            State::Count(count) => count.to_string(),
            State::Goto(count) => format!("{}g", prefix(count)),
            State::Operator { op, count1 } => format!("{}{}", prefix(count1), op.key()),
            State::OperatorCount { op, count1, count2 } => {
                format!("{}{}{}", prefix(count1), op.key(), count2)
            }
            State::OperatorGoto { op, count1 } => format!("{}{}g", prefix(count1), op.key()),
            State::OperatorFind { op, count1, kind } => {
                format!("{}{}{}", prefix(count1), op.key(), kind.key())
            }
            State::Find { kind, count } => {
                format!(
                    "{}{}",
                    prefix(Some(count).filter(|count| *count > 1)),
                    kind.key()
                )
            }
            State::Replace { count } => {
                format!("{}r", prefix(count.filter(|count| *count > 1)))
            }
        }
    }

    /// Feeds one printable key, or returns `Cancel` for an unsupported
    /// continuation. A canceled prefix never executes its trailing key.
    pub fn push(&mut self, ch: char) -> Step {
        match self.state {
            State::Idle => self.start(ch, None),
            State::Count(count) => {
                if ch.is_ascii_digit() {
                    self.state = State::Count(append_digit(count, ch));
                    Step::Waiting
                } else {
                    self.start(ch, Some(count))
                }
            }
            State::Goto(count) => {
                self.state = State::Idle;
                if ch == 'g' {
                    Step::Motion {
                        spec: MotionSpec::GotoFirstLine(count),
                        count,
                    }
                } else {
                    Step::Cancel
                }
            }
            State::Operator { op, count1 } => self.operator_key(op, count1, ch),
            State::OperatorCount { op, count1, count2 } => {
                if ch.is_ascii_digit() {
                    self.state = State::OperatorCount {
                        op,
                        count1,
                        count2: append_digit(count2, ch),
                    };
                    Step::Waiting
                } else {
                    self.operator_key(op, Some(multiply(count1, Some(count2))), ch)
                }
            }
            State::OperatorGoto { op, count1 } => {
                self.state = State::Idle;
                if ch == 'g' {
                    Step::Operator {
                        op,
                        spec: MotionSpec::GotoFirstLine(count1),
                        count: count1,
                    }
                } else {
                    Step::Cancel
                }
            }
            State::OperatorFind { op, count1, kind } => {
                self.state = State::Idle;
                if is_target_char(ch) {
                    Step::OperatorFind {
                        op,
                        kind,
                        target: ch,
                        count: count1.unwrap_or(1),
                    }
                } else {
                    Step::Cancel
                }
            }
            State::Find { kind, count } => {
                self.state = State::Idle;
                if is_target_char(ch) {
                    Step::Find {
                        kind,
                        target: ch,
                        count,
                    }
                } else {
                    Step::Cancel
                }
            }
            State::Replace { count } => {
                self.state = State::Idle;
                if is_target_char(ch) {
                    Step::Replace {
                        target: ReplaceTarget::Char(ch),
                        count,
                    }
                } else {
                    Step::Cancel
                }
            }
        }
    }

    /// Enter while an `r` target is awaited replaces with one line break.
    pub fn push_enter(&mut self) -> Step {
        match self.state {
            State::Replace { count } => {
                self.state = State::Idle;
                Step::Replace {
                    target: ReplaceTarget::LineBreak,
                    count,
                }
            }
            State::Idle => Step::Cancel,
            _ => {
                self.state = State::Idle;
                Step::Cancel
            }
        }
    }

    /// Tab while an `r` target is awaited inserts tab characters.
    pub fn push_tab(&mut self) -> Step {
        match self.state {
            State::Replace { count } => {
                self.state = State::Idle;
                Step::Replace {
                    target: ReplaceTarget::Tab,
                    count,
                }
            }
            State::Idle => Step::Cancel,
            _ => {
                self.state = State::Idle;
                Step::Cancel
            }
        }
    }

    fn start(&mut self, ch: char, count: Option<usize>) -> Step {
        self.state = State::Idle;
        match ch {
            '1'..='9' if count.is_none() => {
                self.state = State::Count(ch.to_digit(10).unwrap_or(1) as usize);
                Step::Waiting
            }
            'd' | 'c' | 'y' => {
                let op = Operator::from_key(ch).expect("operator key");
                self.state = State::Operator { op, count1: count };
                Step::Waiting
            }
            'g' => {
                self.state = State::Goto(count);
                Step::Waiting
            }
            'f' | 'F' | 't' | 'T' => {
                let kind = FindKind::from_key(ch).expect("find key");
                self.state = State::Find {
                    kind,
                    count: count.unwrap_or(1),
                };
                Step::Waiting
            }
            'r' => {
                self.state = State::Replace { count };
                Step::Waiting
            }
            _ => match MotionSpec::from_key(ch) {
                Some(spec) => {
                    let spec = match spec {
                        MotionSpec::LineEnd(_) => MotionSpec::LineEnd(count.unwrap_or(1)),
                        MotionSpec::GotoLastLine(_) => MotionSpec::GotoLastLine(count),
                        other => other,
                    };
                    Step::Motion { spec, count }
                }
                None => Step::Command { key: ch, count },
            },
        }
    }

    fn operator_key(&mut self, op: Operator, count1: Option<usize>, ch: char) -> Step {
        self.state = State::Idle;
        // After an operator, `0` is the line-start motion, not a count digit; a
        // motion count can only start with 1-9. Once a motion count has started,
        // further zeros are digits (handled by the OperatorCount state).
        if ch.is_ascii_digit() && ch != '0' {
            self.state = State::OperatorCount {
                op,
                count1,
                count2: ch.to_digit(10).unwrap_or(0) as usize,
            };
            return Step::Waiting;
        }
        if Operator::from_key(ch) == Some(op) {
            return Step::OperatorLinewise {
                op,
                count: multiply(count1, None),
            };
        }
        match ch {
            'g' => {
                self.state = State::OperatorGoto { op, count1 };
                Step::Waiting
            }
            'f' | 'F' | 't' | 'T' => {
                let kind = FindKind::from_key(ch).expect("find key");
                self.state = State::OperatorFind { op, count1, kind };
                Step::Waiting
            }
            _ => match MotionSpec::from_key(ch) {
                Some(spec) => Step::Operator {
                    op,
                    spec,
                    count: count1,
                },
                None => Step::Cancel,
            },
        }
    }
}

fn is_target_char(ch: char) -> bool {
    !ch.is_control()
}
