/*! This module contains a handwritten [PEG][1] parser for YARA rules.

The parser receives a sequence of tokens produced by the [`Tokenizer`], and
produces a Concrete Syntax-Tree ([`CST`]), also known as a lossless syntax
tree. The CST is initially represented as a stream of [events][`Event`], but
this stream is later converted to a tree using the [rowan][2] create.

This parser is error-tolerant, it is able to parse YARA code that contains
syntax errors. After each error, the parser recovers and keeps parsing the
remaining code. The resulting CST may contain error nodes containing portions
of the code that are not syntactically correct, but anything outside of those
error nodes is valid YARA code.

[1]: https://en.wikipedia.org/wiki/Parsing_expression_grammar
[2]: https://github.com/rust-analyzer/rowan
 */

use indexmap::IndexSet;
#[cfg(feature = "logging")]
use log::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::str::{from_utf8, Utf8Error};

use crate::ast::AST;
use crate::cst::syntax_stream::SyntaxStream;
use crate::cst::SyntaxKind::*;
use crate::cst::{syntax_stream, CST};
use crate::cst::{CSTStream, Event, SyntaxKind};
use crate::parser::token_stream::TokenStream;
use crate::tokenizer::{Token, TokenId, Tokenizer};
use crate::Span;

mod token_stream;

#[cfg(test)]
mod tests;

/// Produces a CST or AST given some YARA source code.
pub struct Parser<'src> {
    pub(crate) parser: ParserImpl<'src>,
}

impl Iterator for Parser<'_> {
    type Item = Event;
    fn next(&mut self) -> Option<Self::Item> {
        self.parser.next()
    }
}

impl<'src> Parser<'src> {
    /// Creates a new parser for the given source code.
    pub fn new(source: &'src [u8]) -> Self {
        Self { parser: ParserImpl::from(Tokenizer::new(source)) }
    }

    /// Returns the source code passed to the parser.
    #[inline]
    pub fn source(&self) -> &'src [u8] {
        self.parser.tokens.source()
    }

    /// Consumes the parser and returns an Abstract Syntax Tree (AST).
    #[inline]
    #[deprecated(since = "1.3.0", note = "use `AST::from(parser)` instead")]
    pub fn into_ast(self) -> AST<'src> {
        AST::from(self)
    }

    /// Consumes the parser and returns a Concrete Syntax Tree (CST).
    ///
    /// NOTE: This API is still unstable and should not be used by
    /// third-party code.
    #[inline]
    #[doc(hidden)]
    pub fn try_into_cst(self) -> Result<CST, Utf8Error> {
        CST::try_from(self)
    }

    /// Consumes the parser and returns a Concrete Syntax Tree (CST) as
    /// a stream of events.
    #[inline]
    #[deprecated(
        since = "1.3.0",
        note = "use `CSTStream::from(parser)` instead"
    )]
    pub fn into_cst_stream(
        self,
    ) -> CSTStream<'src, impl Iterator<Item = Event> + use<'src>> {
        CSTStream::new(self.source(), self.parser)
    }
}

/// Describes the state of the parser.
enum State {
    /// Indicates that the parser is as the start of the input.
    StartOfInput,
    /// Indicates that the parser is at the end of the input.
    EndOfInput,
    /// The parser is OK, it can continue parsing.
    OK,
    /// The parser has failed to parse some portion of the source code. It can
    /// recover from the failure and go back to OK.
    Failure,
    /// The parser is out fuel. See the `fuel` field in [`ParserImpl`] for
    /// details.
    OutOfFuel,
}

/// Internal implementation of the parser. The [`Parser`] type is only a
/// wrapper around this type.
pub(crate) struct ParserImpl<'src> {
    /// Stream from where the parser consumes the input tokens.
    tokens: TokenStream<'src>,

    /// Stream where the parser puts the events that conform the resulting CST.
    output: SyntaxStream,

    /// The current state of the parser.
    state: State,

    /// How deep is the parser into "optional" branches of the grammar. An
    /// optional branch is one that can fail without the whole production
    /// rule failing. For instance, in `A := B? C` the parser can fail while
    /// parsing `B`, but this failure is acceptable because `B` is optional.
    /// Less obvious cases of optional branches are present in alternatives
    /// and the "zero or more" operation (examples: `(A|B)`, `A*`).
    opt_depth: usize,

    /// How deep is the parse into "not" branches of the grammar.
    not_depth: usize,

    /// How deep is the parser into grammar branches.
    #[cfg(feature = "logging")]
    depth: usize,

    /// Hash map where keys are spans within the source code, and values
    /// are tuples containing the ID of the token actually found and a list of
    /// tokens that were expected to match at that span.
    ///
    /// This hash map plays a crucial role in error reporting during parsing.
    /// Consider the following grammar rule:
    ///
    /// `A := a? b`
    ///
    /// Here, the optional token `a` must be followed by the token `b`. This
    /// can be represented (conceptually, not actual code) as:
    ///
    /// ```text
    /// self.start(A)
    ///     .opt(|p| p.expect(a))
    ///     .expect(b)
    ///     .end()
    /// ```
    ///
    /// If we attempt to parse the sequence `cb`, it will fail at `c` because
    /// the rule matches only `ab` and `b`. The error message should be:
    ///
    /// "expecting `a` or `b`, found `c`"
    ///
    /// This error is generated by the `expect(b)` statement. However, the
    /// `expect` function only knows about the `b` token. So, how do we know
    /// that both `a` and `b` are valid tokens at the position where `c` was
    /// found?
    ///
    /// This is where the `expected_token_errors` hash map comes into play. We
    /// know that `a` is also a valid alternative because the `expect(a)`
    /// inside the `opt` was tried and failed. The parser doesn't fail at that
    /// point because `a` is optional, but it records that `a` was expected at
    /// the position of `c`. When `expect(b)` fails later, the parser looks up
    /// any other token (besides `b`) that were expected to match at the
    /// position and produces a comprehensive error message.
    expected_token_errors: FxHashMap<Span, (TokenId, IndexSet<&'static str>)>,

    /// Similar to `expected_token_errors` but tracks the positions where
    /// unexpected tokens were found. This type of error is produced when
    /// [`ParserImpl::not`] is used. This only stores the span were the
    /// unexpected token was found.
    unexpected_token_errors: FxHashSet<Span>,

    /// Errors that are not yet sent to the `output` stream. The purpose of
    /// this vector is removing duplicate messages for the same code span. In
    /// certain cases the parser can produce two different error messages for
    /// the same span, but they won't be added to this vector if another error
    /// with the same span already exists. We don't use a `HashMap` because
    /// the number of items is usually small, and using a vector offers a
    /// better performance.
    pending_errors: Vec<(Span, String)>,

    /// A cache for storing partial parser results. Each item in the set is a
    /// (position, SyntaxKind) tuple, where position is the absolute index
    /// of a token within the source code. The presence of a tuple in the
    /// cache indicates that the non-terminal indicated by SyntaxKind failed
    /// to match that position. Notice that only parser failures are cached,
    /// but successes are not cached. [packrat][1] parsers usually cache both
    /// failure and successes, but we cache only failures because this enough
    /// for speeding up some edge cases, while memory consumption remains low
    /// because we don't need to store the actual result of the parser, only
    /// the fact that if failed.
    ///
    /// [1]: https://en.wikipedia.org/wiki/Packrat_parser
    cache: FxHashSet<(usize, SyntaxKind)>,

    /// This is a mechanism for preventing the parser to take a huge amount of
    /// time parsing pathologically bad inputs. The parser starts with a certain
    /// amount of fuel that is decremented every time it starts parsing a
    /// non-terminal symbol. If the fuel reaches 0, the parsing is aborted.
    fuel: usize,
}

impl<'src> From<Tokenizer<'src>> for ParserImpl<'src> {
    /// Creates a new parser that receives tokens from the given [`Tokenizer`].
    fn from(tokenizer: Tokenizer<'src>) -> Self {
        Self {
            tokens: TokenStream::new(tokenizer),
            output: SyntaxStream::new(),
            pending_errors: Vec::new(),
            expected_token_errors: FxHashMap::default(),
            unexpected_token_errors: FxHashSet::default(),
            cache: FxHashSet::default(),
            opt_depth: 0,
            not_depth: 0,
            #[cfg(feature = "logging")]
            depth: 0,
            state: State::StartOfInput,
            fuel: 100_000_000,
        }
    }
}

/// The parser behaves as an iterator that returns events of type [`Event`].
impl Iterator for ParserImpl<'_> {
    type Item = Event;

    fn next(&mut self) -> Option<Self::Item> {
        match self.state {
            State::StartOfInput => {
                self.state = State::OK;
                Some(Event::Begin {
                    kind: SOURCE_FILE,
                    span: Span(0..self.tokens.source().len() as u32),
                })
            }
            State::EndOfInput => None,
            _ => {
                // If the output buffer isn't empty, return a buffered event.
                if let Some(token) = self.output.pop() {
                    return Some(token);
                }
                // If the output buffer is empty and there are pending tokens, invoke
                // the parser to consume tokens and put more events in the output
                // buffer.
                //
                // Each call to `next` parses one top-level item (either an import
                // statement or rule declaration). This approach parses the source
                // code lazily, one top-level item at a time, saving memory by
                // avoiding tokenizing the entire input at once, or producing all
                // the events before they are consumed.
                if !matches!(self.state, State::OutOfFuel)
                    && self.tokens.has_more()
                {
                    let _ = self.trivia();
                    let _ = self.top_level_item();
                    self.flush_errors();
                    self.cache.clear();
                    self.set_state(State::OK);
                }
                // If still there are no more tokens, we have reached the end of
                // the input.
                if let Some(token) = self.output.pop() {
                    Some(token)
                } else {
                    self.state = State::EndOfInput;
                    Some(Event::End {
                        kind: SOURCE_FILE,
                        span: Span(0..self.tokens.source().len() as u32),
                    })
                }
            }
        }
    }
}

/// Parser private API.
///
/// This section contains utility functions that are used by the grammar rules.
impl<'src> ParserImpl<'src> {
    /// Returns the next token, without consuming it.
    ///
    /// Returns `None` if there are no more tokens.
    fn peek(&mut self) -> Option<&Token> {
        self.tokens.peek_token(0)
    }

    /// Returns the next non-trivia token, without consuming any token.
    ///
    /// Trivia tokens are those that are not really relevant and can be ignored,
    /// like whitespaces, newlines, and comments. This function skips trivia
    /// tokens until finding one that is non-trivia.
    fn peek_non_trivia(&mut self) -> Option<&Token> {
        let mut i = 0;
        // First find the position of the first token that is not a whitespace
        // and then use `peek_token` again for returning it. This is necessary
        // due to a current limitation in the borrow checker that doesn't allow
        // this:
        //
        // loop {
        //     match self.tokens.peek_token(i) {
        //         Some(token) => {
        //             if token.is_trivia() {
        //                 i += 1;
        //             } else {
        //                 return Some(token);
        //             }
        //         }
        //         None => return None,
        //     }
        // }
        //
        let token_pos = loop {
            match self.tokens.peek_token(i) {
                Some(token) => {
                    if token.is_trivia() {
                        i += 1;
                    } else {
                        break i;
                    }
                }
                None => return None,
            }
        };
        self.tokens.peek_token(token_pos)
    }

    /// Consumes the next token and returns it. The consumed token is also
    /// appended to the output.
    ///
    /// Returns `None` if there are no more tokens.
    fn bump(&mut self) -> Option<Token> {
        let token = self.tokens.next_token();
        if let Some(token) = &token {
            self.output.push_token(token.into(), token.span())
        }
        token
    }

    /// Sets a bookmark at the current parser state.
    ///
    /// This saves the current parser state, allowing the parser to try
    /// a grammar production, and if it fails, go back to the saved state
    /// and try a different grammar production.
    fn bookmark(&mut self) -> Bookmark {
        Bookmark {
            tokens: self.tokens.bookmark(),
            output: self.output.bookmark(),
        }
    }

    /// Restores the parser to the state indicated by the bookmark.
    fn restore_bookmark(&mut self, bookmark: &Bookmark) {
        self.tokens.restore_bookmark(&bookmark.tokens);
        self.output.truncate(&bookmark.output);
    }

    /// Removes a bookmark.
    ///
    /// Once a bookmark is removed the parser can't be restored to the
    /// state indicated by the bookmark.
    fn remove_bookmark(&mut self, bookmark: Bookmark) {
        self.tokens.remove_bookmark(bookmark.tokens);
        self.output.remove_bookmark(bookmark.output);
    }

    /// Switches to hex pattern mode.
    fn enter_hex_pattern_mode(&mut self) -> &mut Self {
        if matches!(self.state, State::Failure | State::OutOfFuel) {
            return self;
        }
        self.tokens.enter_hex_pattern_mode();
        self
    }

    /// Switches to hex jump mode.
    fn enter_hex_jump_mode(&mut self) -> &mut Self {
        if matches!(self.state, State::Failure | State::OutOfFuel) {
            return self;
        }
        self.tokens.enter_hex_jump_mode();
        self
    }

    /// Sets the parser state, except if the current state is
    /// [`State::OutOfFuel`], in that  case the state remains unchanged.
    fn set_state(&mut self, state: State) {
        if !matches!(self.state, State::OutOfFuel) {
            self.state = state;
        }
    }

    /// Indicates the start of a non-terminal symbol of a given kind.
    ///
    /// Must be followed by a matching [`Parser::end`].
    fn begin(&mut self, kind: SyntaxKind) -> &mut Self {
        self.trivia();

        #[cfg(feature = "logging")]
        {
            debug!(
                "{}{:?}    -- next token: {}",
                "  ".repeat(self.depth),
                kind,
                self.tokens
                    .peek_token(0)
                    .map(|t| format!("{:?}", t))
                    .unwrap_or_default()
            );
            self.depth += 1;
        }

        if let Some(fuel) = self.fuel.checked_sub(1) {
            self.fuel = fuel;
        } else {
            self.state = State::OutOfFuel;
        }

        self.output.begin(kind);
        self
    }

    /// Indicates the end of the non-terminal symbol that was previously
    /// started with [`Parser::begin`].
    fn end(&mut self) -> &mut Self {
        #[cfg(feature = "logging")]
        {
            self.depth -= 1;
        }
        if matches!(self.state, State::Failure | State::OutOfFuel) {
            self.output.end_with_error();
        } else {
            self.output.end();
        }
        self
    }

    /// Similar to [`Parser::end`] but also recovers the parser from previous
    /// errors, consuming all tokens until finding one that is in the recovery
    /// set.
    fn end_with_recovery(
        &mut self,
        recovery_set: &'static TokenSet,
    ) -> &mut Self {
        if let Some(token) = self.peek_non_trivia() {
            if recovery_set.contains(token).is_some() {
                self.end();
                self.recover();
                return self;
            } else {
                let token_span = token.span();
                let token_id = token.id();

                self.trivia();
                self.bump();
                self.set_state(State::Failure);

                // If there were previous errors, flush those errors and
                // don't produce new ones, but if no previous error exist
                // then create an error that tells that we are expecting
                // any of the tokens in the recovery set.
                if self.pending_errors.is_empty() {
                    let (actual_token_id, expected) = self
                        .expected_token_errors
                        .entry(token_span)
                        .or_default();

                    *actual_token_id = token_id;

                    expected.extend(
                        recovery_set
                            .token_ids()
                            .map(|token| token.description()),
                    );

                    self.handle_errors();
                } else {
                    self.flush_errors();
                }
            }
        }

        while let Some(token) = self.peek_non_trivia() {
            if recovery_set.contains(token).is_some() {
                break;
            } else {
                self.trivia();
                self.bump();
            }
        }

        self.end();
        self.recover();
        self
    }

    /// Sets the parser state to [`State::OK`] if its previous state was
    /// [`State::Failure`].
    fn recover(&mut self) -> &mut Self {
        self.set_state(State::OK);
        self
    }

    /// Consumes trivia tokens until finding one that is non-trivia.
    ///
    /// Trivia tokens those that are not really part of the language, like
    /// whitespaces, newlines and comments.
    fn trivia(&mut self) -> &mut Self {
        if matches!(self.state, State::Failure | State::OutOfFuel) {
            return self;
        }
        while let Some(token) = self.peek() {
            if token.is_trivia() {
                self.bump();
            } else {
                break;
            }
        }
        self
    }

    /// Checks that the next non-trivia token matches one of the expected
    /// tokens.
    ///
    /// If the next non-trivia token does not match any of the expected tokens,
    /// no token will be consumed, the parser will transition to a failure
    /// state and generate an error message. If it matches, the non-trivia
    /// token and any trivia token that appears in front of it will be
    /// consumed and sent to the output.
    fn expect(&mut self, expected_tokens: &'static TokenSet) -> &mut Self {
        self.expect_d(expected_tokens, None)
    }

    /// Like [`ParserImpl::expect`], but allows specifying a custom
    /// description for the expected tokens.
    fn expect_d(
        &mut self,
        expected_tokens: &'static TokenSet,
        description: Option<&'static str>,
    ) -> &mut Self {
        debug_assert!(!expected_tokens.is_empty());

        if matches!(self.state, State::Failure | State::OutOfFuel) {
            return self;
        }

        let (token_id, token_match, token_span) = match self.peek_non_trivia()
        {
            None => {
                // Special case when the end of the source is reached. The span
                // used for error reporting is a zero-length span pointing to
                // last byte in the source code.
                let last = self.tokens.source().len().saturating_sub(1) as u32;
                (None, None, Span(last..last))
            }
            Some(token) => (
                Some(token.id()),
                expected_tokens.contains(token),
                token.span(),
            ),
        };

        match (self.not_depth, token_match) {
            // The expected token was found, but we are inside a "not".
            // When we are inside a "not", any "expect" is negated, and
            // actually means that the token was *not* expected.
            (not_depth, Some(_)) if not_depth > 0 => {
                self.unexpected_token_errors.insert(token_span);
                self.handle_errors()
            }
            // We are not inside a "not", and the expected token was
            // not found.
            (0, None) => {
                let (actual_token_id, expected) =
                    self.expected_token_errors.entry(token_span).or_default();

                *actual_token_id = token_id.unwrap_or(TokenId::UNKNOWN);

                if let Some(description) = description {
                    expected.insert(description);
                } else {
                    expected.extend(
                        expected_tokens
                            .token_ids()
                            .map(|token| token.description()),
                    );
                }

                self.handle_errors();
            }
            _ => {}
        }

        if let Some(t) = token_match {
            // Consume any trivia token in front of the non-trivia expected
            // token.
            self.trivia();
            // Consume the expected token.
            let token = self.tokens.next_token().unwrap();
            self.output.push_token(*t, token.span());
            // After matching a token that is not inside an "optional" branch
            // in the grammar, it's guaranteed that the parser won't go back
            // to a position at the left of the matched token. This is a good
            // opportunity for flushing errors.
            if self.opt_depth == 0 {
                self.flush_errors()
            }
        } else {
            self.set_state(State::Failure);
        }

        self
    }

    /// Begins an alternative.
    ///
    /// # Example
    ///
    /// ```text
    /// p.begin_alt()
    ///  .alt(..)
    ///  .alt(..)
    ///  .end_alt()
    /// ```
    fn begin_alt(&mut self) -> Alt<'_, 'src> {
        let bookmark = self.bookmark();
        Alt { parser: self, matched: false, bookmark }
    }

    /// Applies `parser` optionally.
    ///
    /// If `parser` fails, the failure is ignored and the parser is reset to
    /// its previous state.
    ///
    /// # Example
    ///
    /// ```text
    /// p.opt(|p| p.something_optional())
    /// ```
    fn opt<P>(&mut self, parser: P) -> &mut Self
    where
        P: Fn(&mut Self) -> &mut Self,
    {
        if matches!(self.state, State::Failure | State::OutOfFuel) {
            return self;
        }

        let bookmark = self.bookmark();

        self.trivia();
        self.opt_depth += 1;
        parser(self);
        self.opt_depth -= 1;

        // Any error occurred while parsing the optional production is ignored.
        if matches!(self.state, State::Failure) {
            self.recover();
            self.restore_bookmark(&bookmark);
        }

        self.remove_bookmark(bookmark);
        self
    }

    /// Negates the result of `parser`.
    ///
    /// If `parser` is successful the parser transitions to failure state.
    fn not<P>(&mut self, parser: P) -> &mut Self
    where
        P: Fn(&mut Self) -> &mut Self,
    {
        if matches!(self.state, State::Failure | State::OutOfFuel) {
            return self;
        }

        let bookmark = self.bookmark();

        self.trivia();

        self.not_depth += 1;
        parser(self);
        self.not_depth -= 1;

        self.state = match self.state {
            State::OK => State::Failure,
            State::Failure => State::OK,
            State::OutOfFuel => State::OutOfFuel,
            _ => unreachable!(),
        };

        self.restore_bookmark(&bookmark);
        self.remove_bookmark(bookmark);
        self
    }

    /// Like [`ParserImpl::expect`], but optional.
    fn opt_expect(&mut self, expected_tokens: &'static TokenSet) -> &mut Self {
        self.opt(|p| p.expect(expected_tokens))
    }

    /// If the next non-trivia token matches one of the expected tokens,
    /// consume all trivia tokens and applies `parser`.
    ///
    /// `if_next(TOKEN, |p| p.expect(TOKEN))` is logically equivalent to
    /// `opt(|p| p.expect(TOKEN))`, but the former is more efficient because it
    /// doesn't do any backtracking. The closure `|p| p.expect(TOKEN)` is
    /// executed only after we are sure that the next non-trivia token is
    /// `TOKEN`.
    ///
    /// This can be used for replacing `opt` when the optional production can
    /// be unequivocally distinguished by its first token. For instance, in a
    /// YARA rule the metadata section is optional, but always starts with
    /// the `meta` keyword, so, instead of:
    ///
    /// `opt(|p| p.meta_blk()`)
    ///
    /// We can use:
    ///
    /// `if_next(t!(META_KW), |p| p.meta_blk())`
    ///
    fn if_next<P>(
        &mut self,
        expected_tokens: &'static TokenSet,
        parser: P,
    ) -> &mut Self
    where
        P: Fn(&mut Self) -> &mut Self,
    {
        if matches!(self.state, State::Failure | State::OutOfFuel) {
            return self;
        }
        match self.peek_non_trivia() {
            None => {}
            Some(token) => {
                if expected_tokens.contains(token).is_some() {
                    self.trivia();
                    parser(self);
                } else {
                    let token_span = token.span();
                    let token_id = token.id();

                    let (actual_token, expected) = self
                        .expected_token_errors
                        .entry(token_span)
                        .or_default();

                    *actual_token = token_id;

                    expected.extend(
                        expected_tokens
                            .token_ids()
                            .map(|token| token.description()),
                    );
                }
            }
        }
        self
    }

    /// If the next non-trivia token matches one of the expected tokens,
    /// consume all trivia tokens, consume the expected token, and applies
    /// `parser`.
    ///
    /// This is similar to [`ParserImpl::if_next`], the difference between
    /// both functions reside on how they handle the expected token. `if_next`
    /// leave the expected token in the stream, to be consumed by `parser`,
    /// while `cond` consumes the expected token too.
    fn cond<P>(
        &mut self,
        expected_tokens: &'static TokenSet,
        parser: P,
    ) -> &mut Self
    where
        P: Fn(&mut Self) -> &mut Self,
    {
        self.if_next(expected_tokens, |p| {
            p.expect(expected_tokens).then(|p| parser(p))
        });
        self
    }

    /// Applies `parser` zero or more times.
    #[inline]
    fn zero_or_more<P>(&mut self, parser: P) -> &mut Self
    where
        P: Fn(&mut Self) -> &mut Self,
    {
        self.n_or_more(0, parser)
    }

    /// Applies `parser` one or more times.
    #[inline]
    fn one_or_more<P>(&mut self, parser: P) -> &mut Self
    where
        P: Fn(&mut Self) -> &mut Self,
    {
        self.n_or_more(1, parser)
    }

    /// Applies `parser` N or more times.
    fn n_or_more<P>(&mut self, n: usize, parser: P) -> &mut Self
    where
        P: Fn(&mut Self) -> &mut Self,
    {
        if matches!(self.state, State::Failure | State::OutOfFuel) {
            return self;
        }
        // The first N times that `f` is called it must match.
        for _ in 0..n {
            self.trivia();
            parser(self);
            if matches!(self.state, State::Failure | State::OutOfFuel) {
                return self;
            }
        }
        // If the first N matches were ok, keep matching `f` as much as
        // possible.
        loop {
            let bookmark = self.bookmark();
            self.trivia();
            self.opt_depth += 1;
            parser(self);
            self.opt_depth -= 1;
            if matches!(self.state, State::Failure | State::OutOfFuel) {
                self.recover();
                self.restore_bookmark(&bookmark);
                self.remove_bookmark(bookmark);
                break;
            } else {
                self.remove_bookmark(bookmark);
            }
        }
        self
    }

    /// Applies `parser` exactly one time.
    fn then<P>(&mut self, parser: P) -> &mut Self
    where
        P: Fn(&mut Self) -> &mut Self,
    {
        if matches!(self.state, State::Failure | State::OutOfFuel) {
            return self;
        }
        self.trivia();
        parser(self);
        self
    }

    fn cached<P>(&mut self, kind: SyntaxKind, parser: P) -> &mut Self
    where
        P: Fn(&mut Self) -> &mut Self,
    {
        if matches!(self.state, State::OutOfFuel) {
            return self;
        }

        let start_index = self.tokens.current_token_index();

        if self.cache.contains(&(start_index, kind)) {
            self.set_state(State::Failure);
            return self;
        }

        parser(self);

        if matches!(self.state, State::Failure) {
            self.cache.insert((start_index, kind));
        }

        self
    }

    fn flush_errors(&mut self) {
        self.expected_token_errors.clear();
        self.unexpected_token_errors.clear();
        for (span, error) in self.pending_errors.drain(0..) {
            self.output.push_error(error, span);
        }
    }

    fn handle_errors(&mut self) {
        if self.opt_depth > 0 {
            return;
        }
        // From all errors in expected_token_errors, use the one at the largest
        // offset. If several errors start at the same offset, the last one is
        // used.
        let expected_token = self
            .expected_token_errors
            .drain()
            .max_by_key(|(span, _)| span.start());

        // From all errors in unexpected_token_errors, use the one at the
        // largest offset. If several errors start at the same offset, the last
        // one is used.
        let unexpected_token = self
            .unexpected_token_errors
            .drain()
            .max_by_key(|span| span.start());

        let (span, expected) = match (expected_token, unexpected_token) {
            (Some((e, _)), Some(u)) if u.start() > e.start() => (u, None),
            (None, Some(u)) => (u, None),
            (Some((e, expected)), _) => (e, Some(expected)),
            (None, None) => return,
        };

        // If there's a previous error for the same span, ignore this one.
        if self
            .pending_errors
            .iter()
            .any(|(error_span, _)| error_span.eq(&span))
        {
            return;
        }

        let error_msg = match from_utf8(&self.tokens.source()[span.range()]) {
            Ok(actual_token) => {
                if let Some((actual_token_id, expected)) = expected {
                    // When the token actually found in the source code is
                    // unknown, but starts with /*, it's an unclosed comment.
                    // The token is unknown because the missing */ prevents it
                    // from matching the COMMENT token.
                    if actual_token_id == TokenId::UNKNOWN
                        && actual_token.starts_with("/*")
                    {
                        "unclosed comment".to_string()
                    }
                    // When the token actually found in the source code is
                    // unknown, but starts with ", it's an unclosed literal
                    // string.
                    else if actual_token_id == TokenId::UNKNOWN
                        && actual_token.starts_with('"')
                    {
                        "unclosed literal string".to_string()
                    }
                    // When the token actually found in the source code is
                    // unknown, but starts with /, it's an unclosed regexp.
                    else if actual_token_id == TokenId::UNKNOWN
                        && actual_token.starts_with('/')
                    {
                        "unclosed regular expression".to_string()
                    } else {
                        let (last, all_except_last) =
                            expected.as_slice().split_last().unwrap();

                        match (actual_token.len(), all_except_last.len()) {
                            (0, 0) => {
                                format!("expecting {last}, found end of file")
                            }
                            (l, 0) if l > 15 => format!("expecting {last}"),
                            (_, 0) => {
                                format!("expecting {last}, found `{actual_token}`")
                            }
                            (0, _) => {
                                format!(
                                    "expecting {} or {last}, found end of file",
                                    itertools::join(all_except_last.iter(), ", "),
                                )
                            }
                            (l, _) if l > 15 => format!(
                                "expecting {} or {last}",
                                itertools::join(all_except_last.iter(), ", ")
                            ),
                            (_, _) => format!(
                                "expecting {} or {last}, found `{actual_token}`",
                                itertools::join(all_except_last.iter(), ", "),
                            ),
                        }
                    }
                } else if actual_token.is_empty() {
                    "unexpected end of file".to_string()
                } else {
                    format!("unexpected `{actual_token}`")
                }
            }
            Err(_) => "invalid UTF-8 character".to_string(),
        };

        self.pending_errors.push((span, error_msg));
    }
}

macro_rules! t {
    ($( $tokens:path )|*) => {
       &TokenSet(&[$( $tokens ),*])
    };
}

/// Grammar rules.
///
/// Each function in this section parses a piece of YARA source code. For
/// instance, the `import_stmt` function parses a YARA import statement,
/// `rule_decl` parses a rule declaration, etc. Usually, each function is
/// associated to a non-terminal symbol in the grammar, and the function's
/// code defines the grammar production rule for that symbol.
///
/// Let's use the following grammar rule as an example:
///
/// ```text
/// A := a B (C | D)
/// ```
///
/// `A`, `B`, `C` and `D` are non-terminal symbols, while `a` is a terminal
/// symbol (or token). This rule can be read: `A` is expanded as the token
/// `a` followed by the non-terminal symbol `B`, followed by either `C` or
/// `D`.
///
/// This rule would be expressed as:
///
/// ```text
/// fn A(&mut self) -> &mut Self {
///   self.begin(SyntaxKind::A)
///       .expect(t!(a))
///       .then(|p| p.B())
///       .begin_alt()
///          .alt(|p| p.C())
///          .alt(|p| p.D())
///       .end_alt()
///       .end()
/// }
/// ```
///
/// Also notice the use of `begin_alt` and `end_alt` for enclosing alternatives
/// like `(C | D)`. In PEG parsers the order of alternatives is important, the
/// parser tries them sequentially and accepts the first successful match.
/// Thus, a rule like `( a | a B )` is problematic because `a B` won't ever
/// match. If `a B` matches, then `a` also matches, but `a` has a higher
/// priority and prevents `a B` from matching.
impl ParserImpl<'_> {
    /// Parses a top-level item in YARA source file.
    ///
    /// A top-level item is either an import statement or a rule declaration.
    ///
    /// ```text
    /// TOP_LEVEL_ITEM ::= ( IMPORT_STMT | INCLUDE_STMT | RULE_DECL )
    /// ```
    fn top_level_item(&mut self) -> &mut Self {
        let token = match self.peek() {
            Some(token) => token,
            None => {
                self.set_state(State::Failure);
                return self;
            }
        };
        match token {
            Token::IMPORT_KW(_) => self.import_stmt(),
            Token::INCLUDE_KW(_) => self.include_stmt(),
            Token::GLOBAL_KW(_) | Token::PRIVATE_KW(_) | Token::RULE_KW(_) => {
                self.rule_decl()
            }
            token => {
                let span = token.span();
                self.output.push_error(
                    "expecting import statement or rule definition",
                    span,
                );
                self.output.begin(ERROR);
                while let Some(token) = self.peek_non_trivia() {
                    if matches!(
                        token,
                        Token::GLOBAL_KW(_)
                            | Token::PRIVATE_KW(_)
                            | Token::RULE_KW(_)
                    ) {
                        break;
                    }
                    self.trivia();
                    self.bump();
                }
                self.output.end();
                self.set_state(State::Failure);
                self
            }
        }
    }

    /// Parses an import statement.
    ///
    /// ```text
    /// IMPORT_STMT ::= `import` STRING_LIT
    /// ```
    fn import_stmt(&mut self) -> &mut Self {
        self.begin(IMPORT_STMT)
            .expect(t!(IMPORT_KW))
            .expect(t!(STRING_LIT))
            .end()
    }

    /// Parses an include statement.
    ///
    /// ```text
    /// INCLUDE_STMT ::= `include` STRING_LIT
    /// ```
    fn include_stmt(&mut self) -> &mut Self {
        self.begin(INCLUDE_STMT)
            .expect(t!(INCLUDE_KW))
            .expect(t!(STRING_LIT))
            .end()
    }

    /// Parses a rule declaration.
    ///
    /// ```text
    /// RULE_DECL ::= RULE_MODS? `rule` IDENT `{`
    ///   META_BLK?
    ///   PATTERNS_BLK?
    ///   CONDITION_BLK
    /// `}`
    /// ```
    fn rule_decl(&mut self) -> &mut Self {
        self.begin(RULE_DECL)
            .opt(|p| p.rule_mods())
            .expect(t!(RULE_KW))
            .expect(t!(IDENT))
            .if_next(t!(COLON), |p| p.rule_tags())
            .expect(t!(L_BRACE))
            .if_next(t!(META_KW), |p| p.meta_blk())
            .if_next(t!(STRINGS_KW), |p| p.patterns_blk())
            .then(|p| p.condition_blk())
            .expect(t!(R_BRACE))
            .end_with_recovery(t!(GLOBAL_KW
                | PRIVATE_KW
                | RULE_KW
                | IMPORT_KW
                | INCLUDE_KW))
    }

    /// Parses rule modifiers.
    ///
    /// ```text
    /// RULE_MODS := ( `private` `global`? | `global` `private`? )
    /// ```
    fn rule_mods(&mut self) -> &mut Self {
        self.begin(RULE_MODS)
            .begin_alt()
            .alt(|p| p.expect(t!(PRIVATE_KW)).opt_expect(t!(GLOBAL_KW)))
            .alt(|p| p.expect(t!(GLOBAL_KW)).opt_expect(t!(PRIVATE_KW)))
            .end_alt()
            .end()
    }

    /// Parsers rule tags.
    ///
    /// ```text
    /// RULE_TAGS := `:` IDENT+
    /// ```
    fn rule_tags(&mut self) -> &mut Self {
        self.begin(RULE_TAGS)
            .expect(t!(COLON))
            .one_or_more(|p| p.expect(t!(IDENT)))
            .end_with_recovery(t!(L_BRACE))
    }

    /// Parses metadata block.
    ///
    /// ```text
    /// META_BLK := `meta` `:` META_DEF+
    /// ``
    fn meta_blk(&mut self) -> &mut Self {
        self.begin(META_BLK)
            .expect(t!(META_KW))
            .expect(t!(COLON))
            .one_or_more(|p| p.meta_def())
            .end_with_recovery(t!(STRINGS_KW | CONDITION_KW))
    }

    /// Parses a metadata definition.
    ///
    /// ```text
    /// META_DEF := IDENT `=` (
    ///     `true`      |
    ///     `false`     |
    ///     INTEGER_LIT |
    ///     FLOAT_LIT   |
    ///     STRING_LIT
    /// )
    /// ``
    fn meta_def(&mut self) -> &mut Self {
        self.begin(META_DEF)
            .expect(t!(IDENT))
            .expect(t!(EQUAL))
            .begin_alt()
            .alt(|p| {
                p.opt_expect(t!(MINUS)).expect(t!(INTEGER_LIT | FLOAT_LIT))
            })
            .alt(|p| p.expect(t!(STRING_LIT | TRUE_KW | FALSE_KW)))
            .end_alt()
            .end()
    }

    /// Parses the patterns block.
    ///
    /// ```text
    /// PATTERNS_BLK := `strings` `:` PATTERN_DEF+
    /// ``
    fn patterns_blk(&mut self) -> &mut Self {
        self.begin(PATTERNS_BLK)
            .expect(t!(STRINGS_KW))
            .expect(t!(COLON))
            .one_or_more(|p| p.pattern_def())
            .end_with_recovery(t!(CONDITION_KW))
    }

    /// Parses a pattern definition.
    ///
    /// ```text
    /// PATTERN_DEF := PATTERN_IDENT `=` (
    ///     STRING_LIT  |
    ///     REGEXP      |
    ///     HEX_PATTERN
    /// )
    /// ``
    fn pattern_def(&mut self) -> &mut Self {
        self.begin(PATTERN_DEF)
            .expect(t!(PATTERN_IDENT))
            .expect(t!(EQUAL))
            .begin_alt()
            .alt(|p| p.expect(t!(STRING_LIT)))
            .alt(|p| p.expect(t!(REGEXP)))
            .alt(|p| p.hex_pattern())
            .end_alt()
            .opt(|p| p.pattern_mods())
            .end()
    }

    /// Parses pattern modifiers.
    ///
    /// ```text
    /// PATTERN_MODS := PATTERN_MOD+
    /// ``
    fn pattern_mods(&mut self) -> &mut Self {
        self.begin(PATTERN_MODS).one_or_more(|p| p.pattern_mod()).end()
    }

    /// Parses a pattern modifier.
    ///
    /// ```text
    /// PATTERN_MOD := (
    ///   `ascii`                                                  |
    ///   `wide`                                                   |
    ///   `nocase`                                                 |
    ///   `private`                                                |
    ///   `fullword`                                               |
    ///   `base64` | `base64wide` ( `(` STRING_LIT `)` )?          |
    ///   `xor` (
    ///       `(`
    ///         INTEGER_LIT ( `-` INTEGER_LIT) )?
    ///       `)`
    ///    )?
    /// )
    /// ``
    fn pattern_mod(&mut self) -> &mut Self {
        const DESC: Option<&'static str> = Some("pattern modifier");

        self.begin(PATTERN_MOD)
            .begin_alt()
            .alt(|p| {
                p.expect_d(
                    t!(ASCII_KW
                        | WIDE_KW
                        | NOCASE_KW
                        | PRIVATE_KW
                        | FULLWORD_KW),
                    DESC,
                )
            })
            .alt(|p| {
                p.expect_d(t!(BASE64_KW | BASE64WIDE_KW), DESC)
                    .cond(t!(L_PAREN), |p| {
                        p.expect(t!(STRING_LIT)).expect(t!(R_PAREN))
                    })
            })
            .alt(|p| {
                p.expect_d(t!(XOR_KW), DESC).cond(t!(L_PAREN), |p| {
                    p.expect(t!(INTEGER_LIT))
                        .cond(t!(HYPHEN), |p| p.expect(t!(INTEGER_LIT)))
                        .expect(t!(R_PAREN))
                })
            })
            .end_alt()
            .end()
    }

    /// Parses the hex pattern block.
    ///
    /// ```text
    /// HEX_PATTERN := `{` HEX_SUB_PATTERN `}`
    /// ``
    fn hex_pattern(&mut self) -> &mut Self {
        self.begin(HEX_PATTERN)
            .expect(t!(L_BRACE))
            .enter_hex_pattern_mode()
            .then(|p| p.hex_sub_pattern())
            .expect(t!(R_BRACE))
            .end()
    }

    /// Parses the hex sub pattern block.
    ///
    /// ```text
    /// HEX_SUB_PATTERN :=
    ///   (HEX_BYTE | HEX_ALTERNATIVE) (HEX_JUMP* (HEX_BYTE | HEX_ALTERNATIVE))*
    /// ``
    fn hex_sub_pattern(&mut self) -> &mut Self {
        self.begin(HEX_SUB_PATTERN)
            .begin_alt()
            .alt(|p| p.expect(t!(HEX_BYTE)))
            .alt(|p| p.hex_alternative())
            .end_alt()
            .zero_or_more(|p| {
                p.zero_or_more(|p| p.hex_jump())
                    .begin_alt()
                    .alt(|p| p.expect(t!(HEX_BYTE)))
                    .alt(|p| p.hex_alternative())
                    .end_alt()
            })
            .end()
    }

    /// Parses a hex pattern alternative.
    ///
    /// ```text
    /// HEX_ALTERNATIVE := `(` HEX_SUB_PATTERN ( `|` HEX_SUB_PATTERN )* `)`
    /// ``
    fn hex_alternative(&mut self) -> &mut Self {
        self.begin(HEX_ALTERNATIVE)
            .expect(t!(L_PAREN))
            .then(|p| p.hex_sub_pattern())
            .zero_or_more(|p| p.expect(t!(PIPE)).then(|p| p.hex_sub_pattern()))
            .expect(t!(R_PAREN))
            .end()
    }

    /// Parses a hex jump
    ///
    /// ```text
    /// HEX_JUMP := `[` ( INTEGER_LIT? `-` INTEGER_LIT? | INTEGER_LIT ) `]`
    /// ``
    fn hex_jump(&mut self) -> &mut Self {
        self.begin(HEX_JUMP)
            .expect(t!(L_BRACKET))
            .enter_hex_jump_mode()
            .begin_alt()
            .alt(|p| {
                p.opt_expect(t!(INTEGER_LIT))
                    .expect(t!(HYPHEN))
                    .opt_expect(t!(INTEGER_LIT))
            })
            .alt(|p| p.expect(t!(INTEGER_LIT)))
            .end_alt()
            .expect(t!(R_BRACKET))
            .end()
    }

    /// Parses the condition block.
    ///
    /// ```text
    /// CONDITION_BLK := `condition` `:` BOOLEAN_EXPR
    /// ``
    fn condition_blk(&mut self) -> &mut Self {
        self.begin(CONDITION_BLK)
            .expect(t!(CONDITION_KW))
            .expect(t!(COLON))
            .then(|p| p.boolean_expr())
            .end_with_recovery(t!(R_BRACE))
    }

    /// Parses a boolean expression.
    ///
    /// ```text
    /// BOOLEAN_EXPR := BOOLEAN_TERM ((AND_KW | OR_KW) BOOLEAN_TERM)*
    /// ``
    fn boolean_expr(&mut self) -> &mut Self {
        self.begin(BOOLEAN_EXPR)
            .boolean_term()
            .zero_or_more(|p| {
                p.expect_d(t!(AND_KW | OR_KW), Some("operator"))
                    .then(|p| p.boolean_term())
            })
            .end()
    }

    /// Parses a boolean term.
    ///
    /// ```text
    /// BOOLEAN_TERM := (
    ///    `true`                 |
    ///    `false`                |
    ///    `not` BOOLEAN_TERM     |
    ///    `defined` BOOLEAN_TERM |
    ///    `(` BOOLEAN_EXPR `)`
    /// )
    /// ``
    fn boolean_term(&mut self) -> &mut Self {
        const DESC: Option<&'static str> = Some("expression");

        self.begin(BOOLEAN_TERM)
            .begin_alt()
            .alt(|p| {
                p.expect_d(t!(PATTERN_IDENT), DESC).if_next(
                    t!(AT_KW | IN_KW),
                    |p| {
                        p.begin_alt()
                            .alt(|p| p.expect(t!(AT_KW)).expr())
                            .alt(|p| p.expect(t!(IN_KW)).range())
                            .end_alt()
                    },
                )
            })
            .alt(|p| p.expect_d(t!(TRUE_KW | FALSE_KW), DESC))
            .alt(|p| {
                p.expect_d(t!(NOT_KW | DEFINED_KW), DESC)
                    .then(|p| p.boolean_term())
            })
            .alt(|p| p.for_expr())
            .alt(|p| p.of_expr())
            .alt(|p| p.with_expr())
            .alt(|p| {
                p.expr().zero_or_more(|p| {
                    p.expect_d(
                        t!(EQ
                            | NE
                            | LE
                            | LT
                            | GE
                            | GT
                            | CONTAINS_KW
                            | ICONTAINS_KW
                            | STARTSWITH_KW
                            | ISTARTSWITH_KW
                            | ENDSWITH_KW
                            | IENDSWITH_KW
                            | IEQUALS_KW
                            | MATCHES_KW),
                        DESC,
                    )
                    .then(|p| p.expr())
                })
            })
            .alt(|p| {
                p.expect_d(t!(L_PAREN), DESC)
                    .then(|p| p.boolean_expr())
                    .expect(t!(R_PAREN))
            })
            .end_alt()
            .end()
    }

    /// Parses an expression.
    ///
    /// ```text
    /// EXPR := (
    ///    TERM  ( (arithmetic_op | bitwise_op | `.`) TERM )*
    /// )
    /// ``
    fn expr(&mut self) -> &mut Self {
        self.cached(EXPR, |p| {
            p.begin(EXPR)
                .term()
                .zero_or_more(|p| {
                    p.expect_d(
                        t!(ADD
                            | SUB
                            | MUL
                            | DIV
                            | MOD
                            | SHL
                            | SHR
                            | BITWISE_AND
                            | BITWISE_OR
                            | BITWISE_XOR
                            | DOT),
                        Some("operator"),
                    )
                    .then(|p| p.term())
                })
                .end()
        })
    }

    /// Parses a term.
    ///
    /// ```text
    /// TERM := (
    ///     FUNC_CALL |
    ///     PRIMARY_EXPR
    ///     (
    ///        `[` EXPR `]` | `.` FUNC_CALL
    ///     )?
    /// )
    /// ``
    fn term(&mut self) -> &mut Self {
        self.begin(TERM)
            .begin_alt()
            .alt(|p| p.func_call())
            .alt(|p| {
                p.primary_expr().opt(|p| {
                    p.begin_alt()
                        .alt(|p| {
                            p.expect(t!(L_BRACKET))
                                .expr()
                                .expect(t!(R_BRACKET))
                        })
                        .alt(|p| p.expect(t!(DOT)).then(|p| p.func_call()))
                        .end_alt()
                })
            })
            .end_alt()
            .end()
    }

    /// Parses a function call.
    ///
    /// ```text
    /// FUNC_CALL := IDENT `(` ( BOOLEAN_EXPR (`,` BOOLEAN_EXPR )* )? `)`
    /// ``
    fn func_call(&mut self) -> &mut Self {
        self.begin(FUNC_CALL)
            .expect_d(t!(IDENT), Some("expression"))
            .expect(t!(L_PAREN))
            .opt(|p| {
                p.boolean_expr().zero_or_more(|p| {
                    p.expect(t!(COMMA)).then(|p| p.boolean_expr())
                })
            })
            .expect(t!(R_PAREN))
            .end()
    }

    /// Parses a range.
    ///
    /// ```text
    /// RANGE := `(` EXPR `.` `.` EXPR `)`
    /// ``
    fn range(&mut self) -> &mut Self {
        self.begin(RANGE)
            .expect(t!(L_PAREN))
            .then(|p| p.expr())
            .expect(t!(DOT))
            .expect(t!(DOT))
            .then(|p| p.expr())
            .expect(t!(R_PAREN))
            .end()
    }

    /// Parsers a primary expression.
    ///
    /// ```text
    /// PRIMARY_EXPR := (
    ///     FLOAT_LIT                          |
    ///     INTEGER_LIT                        |
    ///     STRING_LIT                         |
    ///     REGEXP                             |
    ///     `filesize`                         |
    ///     `entrypoint`                       |
    ///     PATTERN_COUNT (`in` RANGE)?        |
    ///     PATTERN_OFFSET (`[` EXPR `]`)?     |
    ///     PATTERN_LENGTH (`[` EXPR `]`)?     |
    ///     `-` TERM                           |
    ///     `~` TERM                           |
    ///     `(` EXPR `)`                       |
    ///     IDENT (`.` IDENT !`(` )*
    /// )
    /// ``
    fn primary_expr(&mut self) -> &mut Self {
        const DESC: Option<&'static str> = Some("expression");

        self.cached(PRIMARY_EXPR, |p| {
            p.begin(PRIMARY_EXPR)
                .begin_alt()
                .alt(|p| {
                    p.expect_d(
                        t!(FLOAT_LIT
                            | INTEGER_LIT
                            | STRING_LIT
                            | REGEXP
                            | FILESIZE_KW
                            | ENTRYPOINT_KW),
                        DESC,
                    )
                })
                .alt(|p| {
                    p.expect_d(t!(PATTERN_COUNT), DESC)
                        .cond(t!(IN_KW), |p| p.range())
                })
                .alt(|p| {
                    p.expect_d(t!(PATTERN_OFFSET | PATTERN_LENGTH), DESC)
                        .cond(t!(L_BRACKET), |p| {
                            p.expr().expect(t!(R_BRACKET))
                        })
                })
                .alt(|p| p.expect_d(t!(MINUS), DESC).then(|p| p.term()))
                .alt(|p| p.expect_d(t!(BITWISE_NOT), DESC).then(|p| p.term()))
                .alt(|p| {
                    p.expect_d(t!(L_PAREN), DESC)
                        .then(|p| p.expr())
                        .expect(t!(R_PAREN))
                })
                .alt(|p| {
                    p.expect_d(t!(IDENT), DESC).zero_or_more(|p| {
                        p.expect(t!(DOT))
                            .expect(t!(IDENT))
                            .not(|p| p.expect(t!(L_PAREN)))
                    })
                })
                .end_alt()
                .end()
        })
    }

    /// Parses `for` expression.
    ///
    /// ```text
    /// FOR_EXPR := `for` QUANTIFIER (
    ///     `of` ( `them` | PATTERN_IDENT_TUPLE ) |
    ///     IDENT ( `,` IDENT )* `in` ITERABLE
    /// )
    /// `:` `(` BOOLEAN_EXPR `)
    /// ``
    fn for_expr(&mut self) -> &mut Self {
        self.begin(FOR_EXPR)
            .expect(t!(FOR_KW))
            .then(|p| p.quantifier())
            .begin_alt()
            .alt(|p| {
                p.expect(t!(OF_KW))
                    .begin_alt()
                    .alt(|p| p.expect(t!(THEM_KW)))
                    .alt(|p| p.pattern_ident_tuple())
                    .end_alt()
            })
            .alt(|p| {
                p.expect(t!(IDENT))
                    .zero_or_more(|p| p.expect(t!(COMMA)).expect(t!(IDENT)))
                    .expect(t!(IN_KW))
                    .then(|p| p.iterable())
            })
            .end_alt()
            .expect(t!(COLON))
            .expect(t!(L_PAREN))
            .then(|p| p.boolean_expr())
            .expect(t!(R_PAREN))
            .end()
    }

    /// Parses `of` expression.
    ///
    /// ```text
    /// OF_EXPR := QUANTIFIER (
    ///     `of` ( `them` | PATTERN_IDENT_TUPLE ) ( `at` EXPR | `in` RANGE )? |
    ///     BOOLEAN_EXPR_TUPLE
    /// )
    /// ``
    fn of_expr(&mut self) -> &mut Self {
        self.begin(OF_EXPR)
            .then(|p| p.quantifier())
            .expect(t!(OF_KW))
            .begin_alt()
            .alt(|p| {
                p.begin_alt()
                    .alt(|p| p.expect(t!(THEM_KW)))
                    .alt(|p| p.pattern_ident_tuple())
                    .end_alt()
                    .if_next(t!(AT_KW | IN_KW), |p| {
                        p.begin_alt()
                            .alt(|p| p.expect(t!(AT_KW)).expr())
                            .alt(|p| p.expect(t!(IN_KW)).range())
                            .end_alt()
                    })
            })
            .alt(|p| {
                p.boolean_expr_tuple().not(|p| p.expect(t!(AT_KW | IN_KW)))
            })
            .end_alt()
            .end()
    }

    /// Parses `with` expression.
    ///
    /// ```text
    /// WITH_EXPR := `with` WITH_DECLS `:` `(` BOOLEAN_EXPR `)`
    /// ```
    fn with_expr(&mut self) -> &mut Self {
        self.begin(WITH_EXPR)
            .expect(t!(WITH_KW))
            .then(|p| p.with_declarations())
            .expect(t!(COLON))
            .expect(t!(L_PAREN))
            .then(|p| p.boolean_expr())
            .expect(t!(R_PAREN))
            .end()
    }

    /// Parses `with` identifiers.
    ///
    /// ```text
    /// WITH_DECLS := WITH_DECL (`,` WITH_DECL)*
    ///
    fn with_declarations(&mut self) -> &mut Self {
        self.begin(WITH_DECLS)
            .then(|p| p.with_declaration())
            .zero_or_more(|p| {
                p.expect(t!(COMMA)).then(|p| p.with_declaration())
            })
            .end()
    }

    /// Parses a `with` declaration.
    ///
    /// ```text
    /// WITH_DECL := IDENT `=` EXPR
    /// ```
    fn with_declaration(&mut self) -> &mut Self {
        self.begin(WITH_DECL)
            .expect(t!(IDENT))
            .expect(t!(EQUAL))
            .then(|p| p.expr())
            .end()
    }

    /// Parses quantifier.
    ///
    /// ```text
    /// QUANTIFIER := (
    ///     `all`                           |
    ///     `none`                          |
    ///     `any`                           |
    ///     (INTEGER_LIT | FLOAT_LIT ) `%`  |
    ///     EXPR !`%`
    /// )
    /// ```
    fn quantifier(&mut self) -> &mut Self {
        self.begin(QUANTIFIER)
            .begin_alt()
            .alt(|p| p.expect(t!(ALL_KW | NONE_KW | ANY_KW)))
            // Quantifier can be either a primary expression followed by a %,
            // or an expression not followed by %. We can't make it an expression
            // followed by an optional % because that leads to ambiguity, as
            // expressions can contain the % operator (mod).
            .alt(|p| p.primary_expr().expect(t!(PERCENT)))
            .alt(|p| p.expr().not(|p| p.expect(t!(PERCENT))))
            .end_alt()
            .end()
    }

    /// Parses iterable.
    ///
    /// ```text
    /// ITERABLE := (
    ///     RANGE              |
    ///     EXPR_TUPLE         |
    ///     EXPR
    /// )
    /// ```
    fn iterable(&mut self) -> &mut Self {
        self.begin(ITERABLE)
            .begin_alt()
            .alt(|p| p.range())
            .alt(|p| p.expr_tuple())
            .alt(|p| p.expr())
            .end_alt()
            .end()
    }

    /// Parses a tuple of boolean expressions.
    ///
    /// ```text
    /// BOOLEAN_EXPR_TUPLE := `(` BOOLEAN_EXPR ( `,` BOOLEAN_EXPR )* `)`
    /// ```
    fn boolean_expr_tuple(&mut self) -> &mut Self {
        self.begin(BOOLEAN_EXPR_TUPLE)
            .expect(t!(L_PAREN))
            .then(|p| p.boolean_expr())
            .zero_or_more(|p| p.expect(t!(COMMA)).then(|p| p.boolean_expr()))
            .expect(t!(R_PAREN))
            .end()
    }

    /// Parses a tuple of expressions.
    ///
    /// ```text
    /// EXPR_TUPLE := `(` EXPR ( `,` EXPR )* `)`
    /// ```
    fn expr_tuple(&mut self) -> &mut Self {
        self.begin(EXPR_TUPLE)
            .expect(t!(L_PAREN))
            .then(|p| p.expr())
            .zero_or_more(|p| p.expect(t!(COMMA)).then(|p| p.expr()))
            .expect(t!(R_PAREN))
            .end()
    }

    /// Parses a tuple of pattern identifiers.
    ///
    /// ```text
    /// PATTERN_IDENT_TUPLE := `(` PATTERN_IDENT `*`? ( `,` PATTERN_IDENT `*`? )* `)`
    /// ```
    fn pattern_ident_tuple(&mut self) -> &mut Self {
        self.begin(PATTERN_IDENT_TUPLE)
            .expect(t!(L_PAREN))
            .expect(t!(PATTERN_IDENT))
            .opt_expect(t!(ASTERISK)) // TODO white spaces between ident and *
            .zero_or_more(|p| {
                p.expect(t!(COMMA))
                    .expect(t!(PATTERN_IDENT))
                    .opt_expect(t!(ASTERISK))
            })
            .expect(t!(R_PAREN))
            .end()
    }
}

struct Bookmark {
    tokens: token_stream::Bookmark,
    output: syntax_stream::Bookmark,
}

/// A set of tokens passed to the [`ParserImpl::expect`]
/// function.
///
/// The set is represented by a list of [`SyntaxKind`].
struct TokenSet(&'static [SyntaxKind]);

impl TokenSet {
    #[inline]
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// If the set contains the give `token`, returns `Some` with the
    /// [`SyntaxKind`] that corresponds to the matching token. Otherwise, it
    /// returns `None`.
    fn contains(&self, token: &Token) -> Option<&SyntaxKind> {
        self.0.iter().find(|t| t.token_id() == token.id())
    }

    /// Returns the token IDs associated to the tokens in the set.
    fn token_ids(&self) -> impl Iterator<Item = TokenId> + 'static {
        self.0.iter().map(move |t| t.token_id())
    }
}

struct Alt<'a, 'src> {
    parser: &'a mut ParserImpl<'src>,
    matched: bool,
    bookmark: Bookmark,
}

impl<'a, 'src> Alt<'a, 'src> {
    fn alt<F>(mut self, f: F) -> Self
    where
        F: Fn(&'a mut ParserImpl<'src>) -> &'a mut ParserImpl<'src>,
    {
        if matches!(self.parser.state, State::Failure | State::OutOfFuel) {
            return self;
        }
        // Don't try to match the current alternative if the parser a previous
        // one already matched.
        if !self.matched {
            self.parser.trivia();
            self.parser.opt_depth += 1;
            self.parser = f(self.parser);
            self.parser.opt_depth -= 1;
            match self.parser.state {
                // The current alternative matched.
                State::OK => {
                    self.matched = true;
                }
                // The current alternative didn't match, restore the token
                // stream to the position it has before trying to match.
                State::Failure => {
                    self.parser.recover();
                    self.parser.restore_bookmark(&self.bookmark);
                }
                State::OutOfFuel => {}
                _ => unreachable!(),
            };
        }
        self
    }

    fn end_alt(self) -> &'a mut ParserImpl<'src> {
        self.parser.remove_bookmark(self.bookmark);
        // If none of the alternatives matched, that's a failure.
        if self.matched {
            self.parser.set_state(State::OK);
        } else {
            self.parser.set_state(State::Failure);
            self.parser.handle_errors();
        };
        self.parser
    }
}
