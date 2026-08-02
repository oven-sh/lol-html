#[macro_use]
mod state_machine;

mod lexer;
mod tag_scanner;
mod tree_builder_simulator;

use self::lexer::Lexer;
pub(crate) use self::lexer::{
    AttributeBuffer, AttributeOutline, Lexeme, LexemeSink, NonTagContentLexeme,
    NonTagContentTokenOutline, TagLexeme, TagTokenOutline,
};
pub(crate) use self::state_machine::{ActionError, ActionResult};
use self::state_machine::{ParseResult, StateMachine, StateMachineBookmark};
pub(crate) use self::tag_scanner::TagHintSink;
use self::tag_scanner::TagScanner;
pub use self::tree_builder_simulator::ParsingAmbiguityError;
use self::tree_builder_simulator::{TreeBuilderFeedback, TreeBuilderSimulator};
use crate::rewriter::RewritingError;
use cfg_if::cfg_if;

// NOTE: tag scanner can implicitly force parser to switch to
// the lexer mode if it fails to get tree builder feedback. It's up
// to consumer to switch the parser back to the tag scan mode in
// the tag handler.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ParserDirective {
    WherePossibleScanForTagsOnly,
    Lex,
}

pub(crate) struct ParserContext<S> {
    output_sink: S,
    tree_builder_simulator: TreeBuilderSimulator,
    /// Amount of bytes consumed by previous calls to `parse()`,
    /// i.e. number of bytes from the start of the document until the start of the current input slice
    previously_consumed_byte_count: usize,
}

pub(crate) trait ParserOutputSink: LexemeSink + TagHintSink {}

// Pub only for integration tests
pub struct Parser<S> {
    lexer: Lexer<S>,
    tag_scanner: TagScanner<S>,
    current_directive: ParserDirective,
    context: ParserContext<S>,
    /// Set when a content handler suspended the dispatch mid-parse (see
    /// [`RewritingError::Suspended`]). Holds the state-machine bookmark
    /// (positions relative to the re-buffered unconsumed tail) that
    /// [`Self::resume`] continues from.
    suspension: Option<StateMachineBookmark>,
}

// public only for integration tests
#[allow(private_bounds, private_interfaces)]
impl<S: ParserOutputSink> Parser<S> {
    #[must_use]
    #[inline(never)]
    pub fn new(output_sink: S, initial_directive: ParserDirective, strict: bool) -> Self {
        let context = ParserContext {
            output_sink,
            previously_consumed_byte_count: 0,
            tree_builder_simulator: TreeBuilderSimulator::new(strict),
        };

        Self {
            lexer: Lexer::new(),
            tag_scanner: TagScanner::new(),
            current_directive: initial_directive,
            context,
            suspension: None,
        }
    }

    // generic methods tend to be inlined, but this one is called from a couple of places,
    // and has cheap-to-pass non-constants args, so it won't benefit from being merged into its callers.
    // It's better to outline it, and let its callers be inlined.
    #[inline(never)]
    pub fn parse(&mut self, input: &[u8], last: bool) -> Result<usize, RewritingError> {
        debug_assert!(self.suspension.is_none());

        let parse_result = match self.current_directive {
            ParserDirective::WherePossibleScanForTagsOnly => {
                self.tag_scanner
                    .run_parsing_loop(&mut self.context, input, last)
            }
            ParserDirective::Lex => self.lexer.run_parsing_loop(&mut self.context, input, last),
        };

        self.handle_parse_result(parse_result, input, last)
    }

    /// `true` if the last `parse`/`resume` call was suspended by a content
    /// handler. The number of bytes it reported as consumed is final; the
    /// rest of the input must be buffered and handed back to [`Self::resume`].
    #[inline]
    pub const fn is_suspended(&self) -> bool {
        self.suspension.is_some()
    }

    /// Continues a suspended parse over the re-buffered unconsumed tail.
    /// Must only be called once the suspended dispatch itself has been
    /// resumed (see `Dispatcher::resume_dispatch`). `directive`, when given,
    /// is the one the suspended `handle_tag` never got to return.
    #[inline(never)]
    pub fn resume(
        &mut self,
        input: &[u8],
        last: bool,
        directive: Option<ParserDirective>,
    ) -> Result<usize, RewritingError> {
        let bookmark = self
            .suspension
            .take()
            .expect("Parser::resume called without a suspension");

        if let Some(directive) = directive {
            self.current_directive = directive;
        }

        trace!(@continue_from_bookmark bookmark, self.current_directive, input);

        let parse_result = match self.current_directive {
            ParserDirective::WherePossibleScanForTagsOnly => self
                .tag_scanner
                .continue_from_bookmark(&mut self.context, input, last, bookmark),
            ParserDirective::Lex => {
                self.lexer
                    .continue_from_bookmark(&mut self.context, input, last, bookmark)
            }
        };

        self.handle_parse_result(parse_result, input, last)
    }

    fn handle_parse_result(
        &mut self,
        mut parse_result: ParseResult,
        input: &[u8],
        last: bool,
    ) -> Result<usize, RewritingError> {
        loop {
            let unboxed = match parse_result {
                Ok(unreachable) => match unreachable {},
                Err(boxed) => *boxed,
            };
            match unboxed {
                ActionError::EndOfInput {
                    consumed_byte_count,
                } => {
                    self.context.previously_consumed_byte_count += consumed_byte_count;
                    return Ok(consumed_byte_count);
                }
                ActionError::Suspended {
                    consumed_byte_count,
                    bookmark,
                } => {
                    self.context.previously_consumed_byte_count += consumed_byte_count;
                    self.suspension = Some(bookmark);
                    return Ok(consumed_byte_count);
                }
                ActionError::ParserDirectiveChangeRequired(new_directive, sm_bookmark) => {
                    self.current_directive = new_directive;

                    trace!(@continue_from_bookmark sm_bookmark, self.current_directive, input);

                    parse_result = match self.current_directive {
                        ParserDirective::WherePossibleScanForTagsOnly => self
                            .tag_scanner
                            .continue_from_bookmark(&mut self.context, input, last, sm_bookmark),
                        ParserDirective::Lex => self.lexer.continue_from_bookmark(
                            &mut self.context,
                            input,
                            last,
                            sm_bookmark,
                        ),
                    };
                }
                ActionError::RewritingError(err) => return Err(err),
                ActionError::Internal(err) => {
                    return Err(RewritingError::ContentHandlerError(err.into()));
                }
            }
        }
    }

    pub fn get_dispatcher(&mut self) -> &mut S {
        &mut self.context.output_sink
    }
}

cfg_if! {
    if #[cfg(feature = "_integration_test")] {
        use crate::html::{LocalNameHash, TextType};

        #[allow(private_bounds)]
        impl<S: ParserOutputSink> Parser<S> {
            pub fn switch_text_type(&mut self, text_type: TextType) {
                match self.current_directive {
                    ParserDirective::WherePossibleScanForTagsOnly => {
                        self.tag_scanner.switch_text_type(text_type);
                    }
                    ParserDirective::Lex => self.lexer.switch_text_type(text_type),
                }
            }

            pub fn set_last_start_tag_name_hash(&mut self, name_hash: LocalNameHash) {
                match self.current_directive {
                    ParserDirective::WherePossibleScanForTagsOnly => {
                        self.tag_scanner.set_last_start_tag_name_hash(name_hash);
                    }
                    ParserDirective::Lex => self.lexer.set_last_start_tag_name_hash(name_hash),
                }
            }
        }
    }
}
