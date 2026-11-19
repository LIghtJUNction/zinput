//! Caller-buffered composition state shared by native input frontends and engines.

use core::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidContext,
    NotFocused,
    AlreadyComposing,
    StaleToken,
    BufferTooSmall,
    InvalidSelection,
    InvalidCandidate,
    SequenceExhausted,
}

impl fmt::Display for Error {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(match self {
            Self::InvalidContext => "input context must be nonzero",
            Self::NotFocused => "no input context is focused",
            Self::AlreadyComposing => "a composition is already active",
            Self::StaleToken => "composition or candidate revision is stale",
            Self::BufferTooSmall => "caller buffer is too small",
            Self::InvalidSelection => "selection must follow UTF-8 byte boundaries",
            Self::InvalidCandidate => "candidate index is out of bounds",
            Self::SequenceExhausted => "composition revision counter is exhausted",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ContextId(u64);

impl ContextId {
    pub const fn new(value: u64) -> Result<Self, Error> {
        if value == 0 {
            Err(Error::InvalidContext)
        } else {
            Ok(Self(value))
        }
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Identity of one accepted preedit revision. Every update returns a fresh token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Token {
    context: ContextId,
    sequence: u64,
}

impl Token {
    pub const fn context(self) -> ContextId {
        self.context
    }

    pub const fn sequence(self) -> u64 {
        self.sequence
    }
}

/// Selection endpoints are UTF-8 byte offsets, including a zero-width caret.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Selection {
    pub start: usize,
    pub end: usize,
}

impl Selection {
    pub const fn caret(offset: usize) -> Self {
        Self {
            start: offset,
            end: offset,
        }
    }

    fn valid(self, text: &str) -> bool {
        self.start <= self.end
            && self.end <= text.len()
            && text.is_char_boundary(self.start)
            && text.is_char_boundary(self.end)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Commit {
    pub context: ContextId,
    pub bytes_written: usize,
}

/// Native applications implement this boundary using their platform transport.
///
/// The context must still designate the intended focused input target. An error
/// may follow partial device delivery; the implementation owns retry semantics.
pub trait TextTarget {
    type Error;

    fn commit_text(&mut self, context: ContextId, text: &str) -> Result<(), Self::Error>;
}

/// A bounded borrowed candidate page tied to the preedit that produced it.
pub struct CandidatePage<'a> {
    token: Token,
    candidates: &'a [&'a str],
    selected: Option<usize>,
}

impl<'a> CandidatePage<'a> {
    pub fn new(
        token: Token,
        candidates: &'a [&'a str],
        selected: Option<usize>,
    ) -> Result<Self, Error> {
        if selected.is_some_and(|index| index >= candidates.len()) {
            return Err(Error::InvalidCandidate);
        }
        Ok(Self {
            token,
            candidates,
            selected,
        })
    }

    pub const fn token(&self) -> Token {
        self.token
    }

    pub fn len(&self) -> usize {
        self.candidates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    pub const fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub fn get(&self, index: usize) -> Result<&'a str, Error> {
        self.candidates
            .get(index)
            .copied()
            .ok_or(Error::InvalidCandidate)
    }
}

/// One focused composition. Storage and capacity are supplied by the host.
///
/// Operations allocate no memory. Rejected updates and short output buffers
/// preserve both state and output. Changing focus, canceling, committing and
/// dropping the state erase the retained preedit bytes. Tokens are scoped to
/// this state instance; hosts must not mix tokens from different instances.
pub struct Composition<'a> {
    storage: &'a mut [u8],
    len: usize,
    selection: Selection,
    focused: Option<ContextId>,
    active: Option<Token>,
    sequence: u64,
}

impl<'a> Composition<'a> {
    pub fn new(storage: &'a mut [u8]) -> Self {
        Self {
            storage,
            len: 0,
            selection: Selection::default(),
            focused: None,
            active: None,
            sequence: 0,
        }
    }

    /// Returns true when focus changed. Repeated focus keeps the composition.
    pub fn set_focus(&mut self, context: Option<ContextId>) -> bool {
        if self.focused == context {
            return false;
        }
        self.clear();
        self.focused = context;
        true
    }

    pub const fn focused(&self) -> Option<ContextId> {
        self.focused
    }

    pub const fn active_token(&self) -> Option<Token> {
        self.active
    }

    pub const fn selection(&self) -> Selection {
        self.selection
    }

    pub fn capacity(&self) -> usize {
        self.storage.len()
    }

    pub fn preedit(&self) -> &str {
        // Only valid UTF-8 from &str is stored, and the borrow excludes mutation.
        core::str::from_utf8(&self.storage[..self.len]).expect("stored preedit is UTF-8")
    }

    pub fn begin(&mut self) -> Result<Token, Error> {
        let context = self.focused.ok_or(Error::NotFocused)?;
        if self.active.is_some() {
            return Err(Error::AlreadyComposing);
        }
        let sequence = self.next_sequence()?;
        let token = Token { context, sequence };
        self.sequence = sequence;
        self.active = Some(token);
        Ok(token)
    }

    pub fn set_preedit(
        &mut self,
        token: Token,
        text: &str,
        selection: Selection,
    ) -> Result<Token, Error> {
        self.check(token)?;
        if text.len() > self.storage.len() {
            return Err(Error::BufferTooSmall);
        }
        if !selection.valid(text) {
            return Err(Error::InvalidSelection);
        }
        let sequence = self.next_sequence()?;
        self.storage[..text.len()].copy_from_slice(text.as_bytes());
        if text.len() < self.len {
            self.storage[text.len()..self.len].fill(0);
        }
        self.len = text.len();
        self.selection = selection;
        let token = Token { sequence, ..token };
        self.sequence = sequence;
        self.active = Some(token);
        Ok(token)
    }

    pub fn cancel(&mut self, token: Token) -> Result<(), Error> {
        self.check(token)?;
        self.clear();
        Ok(())
    }

    pub fn commit(&mut self, token: Token, text: &str, output: &mut [u8]) -> Result<Commit, Error> {
        self.check(token)?;
        if text.len() > output.len() {
            return Err(Error::BufferTooSmall);
        }
        output[..text.len()].copy_from_slice(text.as_bytes());
        self.clear();
        Ok(Commit {
            context: token.context,
            bytes_written: text.len(),
        })
    }

    pub fn commit_preedit(&mut self, token: Token, output: &mut [u8]) -> Result<Commit, Error> {
        self.check(token)?;
        if self.len > output.len() {
            return Err(Error::BufferTooSmall);
        }
        let bytes_written = self.len;
        output[..bytes_written].copy_from_slice(&self.storage[..bytes_written]);
        self.clear();
        Ok(Commit {
            context: token.context,
            bytes_written,
        })
    }

    pub fn commit_candidate(
        &mut self,
        page: &CandidatePage<'_>,
        index: usize,
        output: &mut [u8],
    ) -> Result<Commit, Error> {
        self.check(page.token)?;
        self.commit(page.token, page.get(index)?, output)
    }

    fn next_sequence(&self) -> Result<u64, Error> {
        self.sequence.checked_add(1).ok_or(Error::SequenceExhausted)
    }

    fn check(&self, token: Token) -> Result<(), Error> {
        if self.active != Some(token) || self.focused != Some(token.context) {
            return Err(Error::StaleToken);
        }
        Ok(())
    }

    fn clear(&mut self) {
        self.storage[..self.len].fill(0);
        self.len = 0;
        self.selection = Selection::default();
        self.active = None;
    }
}

impl Drop for Composition<'_> {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhausted_revision_does_not_mutate_preedit() {
        let mut bytes = [0; 8];
        let mut state = Composition::new(&mut bytes);
        state.set_focus(Some(ContextId::new(1).unwrap()));
        let token = state.begin().unwrap();
        let token = state.set_preedit(token, "中", Selection::caret(3)).unwrap();
        state.sequence = u64::MAX;
        assert_eq!(
            state.set_preedit(token, "文", Selection::caret(3)),
            Err(Error::SequenceExhausted)
        );
        assert_eq!(state.preedit(), "中");
        assert_eq!(state.active_token(), Some(token));
    }
}
