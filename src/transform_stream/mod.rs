mod dispatcher;

pub(crate) use self::dispatcher::AuxStartTagInfo;
use self::dispatcher::Dispatcher;
pub use self::dispatcher::DispatcherError;
pub use self::dispatcher::OutputSink;
pub use self::dispatcher::{StartTagHandlingResult, TransformController};
use crate::base::SharedEncoding;
use crate::memory::{Arena, SharedMemoryLimiter};
use crate::parser::{Parser, ParserDirective};
use crate::rewriter::RewritingError;

// Pub only for integration tests
/// Construction parameters for a [`TransformStream`].
pub struct TransformStreamSettings<C, O>
where
    C: TransformController,
    O: OutputSink,
{
    /// Receives tag decisions and captured tokens.
    pub transform_controller: C,
    /// Receives the (possibly rewritten) output; see
    /// [`TransformController::should_emit_content`].
    pub output_sink: O,
    /// Initial size of the buffer that holds input carried across chunks.
    pub preallocated_parsing_buffer_size: usize,
    /// Bounds that buffer (and other per-stream allocations).
    pub memory_limiter: SharedMemoryLimiter,
    /// Input encoding; must be ASCII-compatible.
    pub encoding: SharedEncoding,
    /// Fail on constructs whose parsing depends on scripting/tree state the
    /// tokenizer cannot know (see [`Settings::strict`](crate::Settings)).
    pub strict: bool,
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
/// The streaming HTML tokenizer driving a [`TransformController`]: feed it
/// input with [`write`](Self::write) and finish with [`end`](Self::end).
pub struct TransformStream<C, O>
where
    C: TransformController,
    O: OutputSink,
{
    parser: Parser<Dispatcher<C, O>>,
    buffer: Arena,
    has_buffered_data: bool,
    suspended: SuspendedPhase,
}

impl<C, O> TransformStream<C, O>
where
    C: TransformController,
    O: OutputSink,
{
    /// Creates a stream; nothing is parsed until [`write`](Self::write).
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
            suspended: SuspendedPhase::None,
        }
    }

    /// The transform controller this stream was created with.
    #[inline]
    pub fn controller(&mut self) -> &mut C {
        self.parser.get_dispatcher().transform_controller_mut()
    }

    /// Parses the next chunk of input. Input that cannot be tokenized yet (a
    /// tag split across chunks) is buffered until the next call.
    pub fn write(&mut self, data: &[u8]) -> Result<(), RewritingError> {
        trace!(@write data);
        debug_assert!(self.suspended == SuspendedPhase::None);

        let chunk = if self.has_buffered_data {
            self.buffer
                .append(data)
                .map_err(RewritingError::MemoryLimitExceeded)?;

            self.buffer.bytes()
        } else {
            data
        };

        trace!(@chunk chunk);

        let consumed_byte_count = self.parser.parse(chunk, false)?;

        self.parser
            .get_dispatcher()
            .flush_remaining_input(chunk, consumed_byte_count);

        if consumed_byte_count < chunk.len() {
            if self.has_buffered_data {
                self.buffer.shift(consumed_byte_count);
            } else if let Some(unconsumed) = data.get(consumed_byte_count..) {
                self.buffer
                    .init_with(unconsumed)
                    .map_err(RewritingError::MemoryLimitExceeded)?;

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

    /// Declares the input complete: flushes buffered text and runs
    /// [`TransformController::handle_end`].
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
        let consumed_byte_count = self.parser.parse(chunk, true)?;

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
                let consumed_byte_count = self.parser.resume(chunk, last, directive)?;

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

    #[cfg(feature = "integration_test")]
    #[allow(private_interfaces)]
    pub fn parser(&mut self) -> &mut Parser<Dispatcher<C, O>> {
        &mut self.parser
    }
}
