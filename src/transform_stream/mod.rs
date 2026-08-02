mod dispatcher;

use self::dispatcher::Dispatcher;
pub use self::dispatcher::OutputSink;
pub(crate) use self::dispatcher::{AuxStartTagInfo, DispatcherError};
pub use self::dispatcher::{StartTagHandlingResult, TransformController};
use crate::AsciiCompatibleEncoding;
use crate::base::SharedEncoding;
use crate::memory::{Arena, SharedMemoryLimiter};
use crate::parser::{Parser, ParserDirective};
use crate::rewriter::RewritingError;

// Pub only for integration tests
pub struct TransformStreamSettings<C, O>
where
    C: TransformController,
    O: OutputSink,
{
    pub transform_controller: C,
    pub output_sink: O,
    pub preallocated_parsing_buffer_size: usize,
    pub memory_limiter: SharedMemoryLimiter,
    pub encoding: AsciiCompatibleEncoding,
    pub next_encoding: SharedEncoding,
    pub strict: bool,
    pub graceful_bail_out_on_memory_limit_exceeded: bool,
    pub graceful_bail_out_on_content_handler_error: bool,
}

/// Which stage of the rewrite a content handler suspended, if any.
#[derive(Copy, Clone, PartialEq, Eq)]
enum SuspendedPhase {
    /// Not suspended.
    None,
    /// `write()`'s parse suspended.
    Write,
    /// `end()`'s (final) parse suspended.
    EndParse,
    /// `end()`'s parse completed but a document-end handler suspended.
    Finish,
}

// Pub only for integration tests
pub struct TransformStream<C, O>
where
    C: TransformController,
    O: OutputSink,
{
    parser: Parser<Dispatcher<C, O>>,
    buffer: Arena,
    has_buffered_data: bool,
    graceful_bail_out_on_memory_limit_exceeded: bool,
    graceful_bail_out_on_content_handler_error: bool,
    suspended: SuspendedPhase,
}

impl<C, O> TransformStream<C, O>
where
    C: TransformController,
    O: OutputSink,
{
    pub fn new(settings: TransformStreamSettings<C, O>) -> Self {
        let initial_parser_directive = if settings
            .transform_controller
            .initial_capture_flags()
            .is_empty()
        {
            ParserDirective::WherePossibleScanForTagsOnly
        } else {
            ParserDirective::Lex
        };

        let dispatcher = Dispatcher::new(
            settings.transform_controller,
            settings.output_sink,
            settings.encoding,
            settings.next_encoding,
        );

        let buffer = Arena::new(
            settings.memory_limiter,
            settings.preallocated_parsing_buffer_size,
        );

        let parser = Parser::new(dispatcher, initial_parser_directive, settings.strict);

        Self {
            parser,
            buffer,
            has_buffered_data: false,
            graceful_bail_out_on_memory_limit_exceeded: settings
                .graceful_bail_out_on_memory_limit_exceeded,
            graceful_bail_out_on_content_handler_error: settings
                .graceful_bail_out_on_content_handler_error,
            suspended: SuspendedPhase::None,
        }
    }

    /// Returns whether the current settings allow bailing out gracefully on `err`. Memory and
    /// content-handler errors are gated by independent flags; parsing-ambiguity errors are
    /// never recovered from (the whole point of strict mode is to refuse uncertain markup).
    fn should_bail_out_for(&self, err: &RewritingError) -> bool {
        match err {
            RewritingError::MemoryLimitExceeded(_) => {
                self.graceful_bail_out_on_memory_limit_exceeded
            }
            RewritingError::ContentHandlerError(_) => {
                self.graceful_bail_out_on_content_handler_error
            }
            RewritingError::ParsingAmbiguity(_) => false,
            // A suspension is not a failure: the rewrite continues from `resume()`.
            RewritingError::Suspended => false,
        }
    }

    pub fn write(&mut self, data: &[u8]) -> Result<(), RewritingError> {
        trace!(@write data);
        debug_assert!(self.suspended == SuspendedPhase::None);

        let chunk = if self.has_buffered_data {
            match self.buffer.append(data) {
                Ok(()) => self.buffer.bytes(),
                Err(e) => {
                    // We can't fit `data` next to the buffered (still-unparsed) bytes from
                    // previous calls. Neither chunk has been emitted to the sink yet, so on a
                    // graceful bail-out we flush both as-is and let the caller continue the
                    // response from where they were.
                    let err = RewritingError::MemoryLimitExceeded(e);

                    if self.should_bail_out_for(&err) {
                        let dispatcher = self.parser.get_dispatcher();
                        dispatcher.run_bail_out_handlers(&err);
                        dispatcher.flush_for_bail_out(self.buffer.bytes());
                        dispatcher.flush_for_bail_out(data);
                    }

                    return Err(err);
                }
            }
        } else {
            data
        };

        trace!(@chunk chunk);

        let consumed_byte_count = match self.parser.parse(chunk, false) {
            Ok(c) => c,
            Err(e) => {
                // The parser failed mid-chunk. The dispatcher's `remaining_content_start`
                // points to the first byte of `chunk` that hasn't been emitted yet (memory
                // errors happen before `lexeme_consumed()`; content handler errors happen
                // between `emit_chunk_before_lexeme()` and `consume_lexeme()`). Flushing from
                // there preserves all bytes the caller fed us.
                if self.should_bail_out_for(&e) {
                    let dispatcher = self.parser.get_dispatcher();
                    dispatcher.run_bail_out_handlers(&e);
                    dispatcher.flush_for_bail_out(chunk);
                }

                return Err(e);
            }
        };

        self.parser
            .get_dispatcher()
            .flush_remaining_input(chunk, consumed_byte_count);

        if consumed_byte_count < chunk.len() {
            if self.has_buffered_data {
                self.buffer.shift(consumed_byte_count);
            } else if let Some(unconsumed) = data.get(consumed_byte_count..) {
                if let Err(e) = self.buffer.init_with(unconsumed) {
                    // Parsing succeeded but we can't buffer the leftover bytes for the next
                    // call. On a graceful bail-out we flush the leftover raw so the response
                    // stays whole.
                    let err = RewritingError::MemoryLimitExceeded(e);

                    if self.should_bail_out_for(&err) {
                        let dispatcher = self.parser.get_dispatcher();
                        dispatcher.run_bail_out_handlers(&err);
                        dispatcher.flush_for_bail_out(unconsumed);
                    }

                    return Err(err);
                }

                self.has_buffered_data = true;
            } else {
                debug_assert!(false);
            }
        } else {
            self.has_buffered_data = false;
        }

        // NOTE: a suspension reports `consumed_byte_count` exactly like an
        // end-of-input, so the tail bookkeeping above already saved the
        // unconsumed rest of the chunk for `resume`.
        if self.parser.is_suspended() {
            self.suspended = SuspendedPhase::Write;
            return Err(RewritingError::Suspended);
        }

        Ok(())
    }

    pub fn end(&mut self) -> Result<(), RewritingError> {
        trace!(@end);
        debug_assert!(self.suspended == SuspendedPhase::None);

        let chunk = if self.has_buffered_data {
            self.buffer.bytes()
        } else {
            &[]
        };

        trace!(@chunk chunk);

        let total = chunk.len();
        let consumed_byte_count = match self.parser.parse(chunk, true) {
            Ok(c) => c,
            Err(e) => {
                // Same reasoning as in `write()`: if we can bail out gracefully, make sure the
                // sink has all the input bytes before propagating the error.
                if self.should_bail_out_for(&e) {
                    let dispatcher = self.parser.get_dispatcher();
                    dispatcher.run_bail_out_handlers(&e);
                    dispatcher.flush_for_bail_out(chunk);
                }

                return Err(e);
            }
        };

        if self.parser.is_suspended() {
            // `finish` (which flushes the raw tail) won't run; emit the
            // consumed part now and keep the rest for `resume`.
            self.parser
                .get_dispatcher()
                .flush_remaining_input(chunk, consumed_byte_count);

            if consumed_byte_count < total {
                self.buffer.shift(consumed_byte_count);
            } else {
                self.has_buffered_data = false;
            }

            self.suspended = SuspendedPhase::EndParse;
            return Err(RewritingError::Suspended);
        }

        // `finish()` flushes any remaining input *first* and only then calls `handle_end()`,
        // so a `ContentHandlerError` from the end handler arrives after the sink already has
        // every input byte. No additional flush needed; the caller continues from where the
        // rewriter left off.
        match self.parser.get_dispatcher().finish(chunk) {
            Err(RewritingError::Suspended) => {
                self.suspended = SuspendedPhase::Finish;
                Err(RewritingError::Suspended)
            }
            res => res,
        }
    }

    /// `true` if a content handler suspended the last `write()`/`end()`/
    /// `resume()` call.
    pub const fn is_suspended(&self) -> bool {
        !matches!(self.suspended, SuspendedPhase::None)
    }

    /// Continues a rewrite that a content handler suspended.
    ///
    /// Returns `Err(RewritingError::Suspended)` again if another handler
    /// suspends. If the suspension happened during `write()`, the caller
    /// still has to call `end()` once this returns `Ok`.
    pub fn resume(&mut self) -> Result<(), RewritingError> {
        match self.suspended {
            SuspendedPhase::None => {
                debug_assert!(false, "TransformStream::resume without a suspension");
                Ok(())
            }
            SuspendedPhase::Write | SuspendedPhase::EndParse => {
                let last = self.suspended == SuspendedPhase::EndParse;

                // 1. Complete the parked dispatch. If another handler
                //    suspends here, the parser bookmark and the buffered
                //    tail are untouched; the phase stays as-is.
                let directive = self.parser.get_dispatcher().resume_dispatch()?;

                // 2. Continue the parse over the tail that was buffered at
                //    suspension time (positions in the bookmark are
                //    relative to it).
                let chunk: &[u8] = if self.has_buffered_data {
                    self.buffer.bytes()
                } else {
                    &[]
                };

                trace!(@chunk chunk);

                let total = chunk.len();
                let consumed_byte_count = match self.parser.resume(chunk, last, directive) {
                    Ok(c) => c,
                    Err(e) => {
                        // Same reasoning as in `write()`.
                        if self.should_bail_out_for(&e) {
                            let dispatcher = self.parser.get_dispatcher();
                            dispatcher.run_bail_out_handlers(&e);
                            dispatcher.flush_for_bail_out(chunk);
                        }

                        return Err(e);
                    }
                };

                self.parser
                    .get_dispatcher()
                    .flush_remaining_input(chunk, consumed_byte_count);

                if consumed_byte_count < total {
                    self.buffer.shift(consumed_byte_count);
                } else {
                    self.has_buffered_data = false;
                }

                if self.parser.is_suspended() {
                    return Err(RewritingError::Suspended);
                }

                self.suspended = SuspendedPhase::None;

                if !last {
                    return Ok(());
                }

                // The final parse is done; run the document-end handlers.
                let chunk: &[u8] = if self.has_buffered_data {
                    self.buffer.bytes()
                } else {
                    &[]
                };

                match self.parser.get_dispatcher().finish(chunk) {
                    Err(RewritingError::Suspended) => {
                        self.suspended = SuspendedPhase::Finish;
                        Err(RewritingError::Suspended)
                    }
                    res => res,
                }
            }
            SuspendedPhase::Finish => match self.parser.get_dispatcher().resume_finish() {
                Err(RewritingError::Suspended) => Err(RewritingError::Suspended),
                res => {
                    self.suspended = SuspendedPhase::None;
                    res
                }
            },
        }
    }

    /// The transform controller, for reaching the parked token of a
    /// suspension from the embedder.
    pub(crate) fn controller_mut(&mut self) -> &mut C {
        self.parser.get_dispatcher().transform_controller_mut()
    }

    #[cfg(feature = "_integration_test")]
    #[allow(private_interfaces)]
    pub fn parser(&mut self) -> &mut Parser<Dispatcher<C, O>> {
        &mut self.parser
    }
}
