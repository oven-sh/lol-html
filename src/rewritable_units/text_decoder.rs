use crate::base::{Bytes, SharedEncoding, SourceLocation, Spanned};
use crate::rewriter::RewritingError;
use encoding_rs::{CoderResult, Decoder, Encoding, UTF_8};

const DEFAULT_BUFFER_LEN: usize = if cfg!(test) { 13 } else { 1024 };

pub(crate) struct TextDecoder {
    encoding: SharedEncoding,
    pending_source_location_bytes_start: usize,
    pending_text_streaming_decoder: Option<Decoder>,
    text_buffer: String,
    /// Where the current `feed_text` call was when a text handler suspended
    /// (`None` otherwise). Recorded *only* on the error path so the success
    /// path pays nothing.
    suspended: Option<TextFeedSuspension>,
}

/// Where a [`TextDecoder::feed_text`] call was when the text handler for one
/// of its chunks suspended. Lets [`TextDecoder::resume_feed`] finish decoding
/// the rest of that text lexeme once the parked chunk completes. The
/// undecoded tail is copied because it pointed into the `write()` call's
/// input buffer, which is gone by the time the resume runs.
pub(crate) struct TextFeedSuspension {
    remaining: Vec<u8>,
    next_source_location_bytes_start: usize,
    last_in_text_node: bool,
    /// `true` if the chunk that suspended was this feed's final one; the
    /// resume then only has to run the loop's epilogue.
    finished: bool,
}

pub(crate) type OutputHandlerCallback<'tmp> =
    dyn FnMut(&str, bool, &'static Encoding, SourceLocation) -> Result<(), RewritingError> + 'tmp;

impl TextDecoder {
    #[inline]
    #[must_use]
    pub fn new(encoding: SharedEncoding) -> Self {
        Self {
            pending_source_location_bytes_start: 0,
            encoding,
            pending_text_streaming_decoder: None,
            // this will be later initialized to DEFAULT_BUFFER_LEN,
            // because encoding_rs wants a slice
            text_buffer: String::new(),
            suspended: None,
        }
    }

    #[inline]
    pub fn flush_pending(
        &mut self,
        output_handler: &mut OutputHandlerCallback<'_>,
    ) -> Result<(), RewritingError> {
        if self.pending_text_streaming_decoder.is_some() {
            self.feed_text(
                Spanned::new(self.pending_source_location_bytes_start, Bytes::new(&[])),
                true,
                output_handler,
            )?;
        }
        Ok(())
    }

    /// The feed state recorded when a text handler suspended, if any.
    #[inline]
    pub fn take_suspended(&mut self) -> Option<TextFeedSuspension> {
        self.suspended.take()
    }

    /// Continues a `feed_text` call that a text handler suspended, starting
    /// right after the chunk that suspended.
    pub fn resume_feed(
        &mut self,
        suspension: TextFeedSuspension,
        output_handler: &mut OutputHandlerCallback<'_>,
    ) -> Result<(), RewritingError> {
        if suspension.finished {
            // The chunk that suspended was this feed's final one: only the
            // loop epilogue is left.
            if suspension.last_in_text_node {
                self.pending_text_streaming_decoder = None;
            } else {
                self.pending_source_location_bytes_start =
                    suspension.next_source_location_bytes_start;
            }
            return Ok(());
        }

        self.feed_decoder_loop(
            &suspension.remaining,
            suspension.next_source_location_bytes_start,
            suspension.last_in_text_node,
            output_handler,
        )
    }

    #[inline(never)]
    pub fn feed_text(
        &mut self,
        input_span: Spanned<Bytes<'_>>,
        last_in_text_node: bool,
        output_handler: &mut OutputHandlerCallback<'_>,
    ) -> Result<(), RewritingError> {
        let mut raw_input = input_span.as_slice();
        let mut next_source_location_bytes_start = input_span.source_location().bytes().start;

        let encoding = self.encoding.get();

        if let Some((utf8_text, rest)) = self.split_utf8_start(raw_input, encoding) {
            raw_input = rest;
            let really_last = last_in_text_node && rest.is_empty();

            let source_location =
                SourceLocation::from_start_len(next_source_location_bytes_start, utf8_text.len());
            next_source_location_bytes_start = source_location.bytes().end;

            let res = (output_handler)(utf8_text, really_last, encoding, source_location);
            if res.is_err() {
                self.suspended = Some(TextFeedSuspension {
                    remaining: rest.to_vec(),
                    next_source_location_bytes_start,
                    last_in_text_node,
                    finished: really_last,
                });
            }
            res?;

            if really_last {
                debug_assert!(self.pending_text_streaming_decoder.is_none());
                return Ok(());
            }
        }

        self.feed_decoder_loop(
            raw_input,
            next_source_location_bytes_start,
            last_in_text_node,
            output_handler,
        )
    }

    /// The streaming-decoder tail of [`Self::feed_text`], also the re-entry
    /// point for [`Self::resume_feed`].
    fn feed_decoder_loop(
        &mut self,
        mut raw_input: &[u8],
        mut next_source_location_bytes_start: usize,
        last_in_text_node: bool,
        output_handler: &mut OutputHandlerCallback<'_>,
    ) -> Result<(), RewritingError> {
        let encoding = self.encoding.get();

        if self.pending_text_streaming_decoder.is_none() && self.text_buffer.is_empty() {
            // repeat() avoids utf8 check comapred to `String::from_utf8(vec![0; len])`
            self.text_buffer = "\0".repeat(DEFAULT_BUFFER_LEN);
        }
        let decoder = self
            .pending_text_streaming_decoder
            .get_or_insert_with(|| encoding.new_decoder_without_bom_handling());

        loop {
            let buffer = self.text_buffer.as_mut_str();
            let (status, read, written, ..) =
                decoder.decode_to_str(raw_input, buffer, last_in_text_node);

            let finished_decoding = status == CoderResult::InputEmpty;
            let source_location =
                SourceLocation::from_start_len(next_source_location_bytes_start, read);
            next_source_location_bytes_start = source_location.bytes().end;

            if written > 0 || last_in_text_node {
                // the last call to feed_text() may make multiple calls to output_handler,
                // but only one call to output_handler can be *the* last one.
                let really_last = last_in_text_node && finished_decoding;

                let res = (output_handler)(
                    // this will always be in bounds, but unwrap_or_default optimizes better
                    buffer.get(..written).unwrap_or_default(),
                    really_last,
                    encoding,
                    source_location,
                );
                if res.is_err() {
                    self.suspended = Some(TextFeedSuspension {
                        remaining: raw_input.get(read..).unwrap_or_default().to_vec(),
                        next_source_location_bytes_start,
                        last_in_text_node,
                        finished: finished_decoding,
                    });
                }
                res?;
            }

            if finished_decoding {
                if last_in_text_node {
                    self.pending_text_streaming_decoder = None;
                } else {
                    self.pending_source_location_bytes_start = next_source_location_bytes_start;
                }
                return Ok(());
            }
            raw_input = raw_input.get(read..).unwrap_or_default();
        }
    }

    /// Fast path for UTF-8 or ASCII prefix
    ///
    /// Returns UTF-8 text to emit + remaining bytes, or `None` if the fast path is not available
    #[inline]
    fn split_utf8_start<'i>(
        &self,
        raw_input: &'i [u8],
        encoding: &'static Encoding,
    ) -> Option<(&'i str, &'i [u8])> {
        // Can't use the fast path if the decoder may have buffered some bytes
        if self.pending_text_streaming_decoder.is_some() {
            return None;
        }

        let text_or_len = if encoding == UTF_8 {
            std::str::from_utf8(raw_input).map_err(|err| err.valid_up_to())
        } else {
            debug_assert!(encoding.is_ascii_compatible());
            Err(Encoding::ascii_valid_up_to(raw_input))
        };

        match text_or_len {
            Ok(utf8_text) => Some((utf8_text, &[][..])),
            Err(valid_up_to) => {
                // The slow path buffers 1KB, and even though this shouldn't matter,
                // it is an observable behavior, and it makes bugs worse for text handlers
                // that assume they'll get only a single chunk.
                if valid_up_to != raw_input.len() && valid_up_to < DEFAULT_BUFFER_LEN {
                    return None;
                }

                let (text, rest) = raw_input.split_at_checked(valid_up_to)?;
                Some((std::str::from_utf8(text).ok()?, rest))
            }
        }
    }
}
