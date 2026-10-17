use crate::base::{Bytes, SharedEncoding, SourceLocation, Spanned};
use crate::rewriter::RewritingError;
use encoding_rs::{CoderResult, Decoder, Encoding, UTF_8};
use std::mem;

const DEFAULT_BUFFER_LEN: usize = if cfg!(test) { 13 } else { 1024 };

pub(crate) struct TextDecoder {
    encoding: SharedEncoding,
    pending_source_location_bytes_start: usize,
    pending_text_streaming_decoder: Option<Decoder>,
    text_buffer: String,
    /// Where the current `feed_text` call was when a text handler suspended
    /// (`None` otherwise). Recorded *only* on the error path so the success
    /// path pays nothing. `resume_feed` keeps its record here while it runs.
    suspended: Option<TextFeedSuspension>,
}

/// Where a [`TextDecoder::feed_text`] call was when the text handler for one
/// of its chunks suspended. Lets [`TextDecoder::resume_feed`] finish decoding
/// the rest of that text lexeme once the parked chunk completes. The
/// undecoded tail is copied because it pointed into the `write()` call's
/// input buffer, which is gone by the time the resume runs. It is copied
/// once per lexeme: when a later chunk of the same lexeme suspends, the same
/// buffer is parked again and only `offset` moves.
pub(crate) struct TextFeedSuspension {
    remaining: Box<[u8]>,
    /// Where the undecoded bytes start in `remaining`.
    offset: usize,
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
        mut suspension: TextFeedSuspension,
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

        // The loop gets the bytes on loan. The record waits in the slot
        // without them, with `offset` at their end. A chunk that suspends
        // again finds the record there (see `park`) and moves `offset` back
        // to the unread tail, so the rest is not copied a second time.
        let remaining = mem::take(&mut suspension.remaining);
        let start = mem::replace(&mut suspension.offset, remaining.len());
        let next_source_location_bytes_start = suspension.next_source_location_bytes_start;
        let last_in_text_node = suspension.last_in_text_node;
        self.suspended = Some(suspension);

        let res = self.feed_decoder_loop(
            remaining.get(start..).unwrap_or_default(),
            next_source_location_bytes_start,
            last_in_text_node,
            output_handler,
        );

        match &mut self.suspended {
            Some(parked) if matches!(res, Err(RewritingError::Suspended)) => {
                parked.remaining = remaining;
            }
            _ => self.suspended = None,
        }

        res
    }

    /// Records where the feed stopped when the text handler for a chunk
    /// suspended. `unread` is the input that the decoder has not read yet.
    /// Any other error ends the rewrite, so nothing is recorded for it.
    #[cold]
    #[inline(never)]
    fn park(
        slot: &mut Option<TextFeedSuspension>,
        res: &Result<(), RewritingError>,
        unread: &[u8],
        next_source_location_bytes_start: usize,
        last_in_text_node: bool,
        finished: bool,
    ) {
        if !matches!(res, Err(RewritingError::Suspended)) {
            return;
        }

        let (remaining, offset) = match slot.take() {
            // `resume_feed` left its record here: `unread` is the tail of
            // the bytes it owns, and it puts them back into the new record.
            Some(lent) => {
                debug_assert!(lent.remaining.is_empty());
                (lent.remaining, lent.offset - unread.len())
            }
            None => (Box::from(unread), 0),
        };

        *slot = Some(TextFeedSuspension {
            remaining,
            offset,
            next_source_location_bytes_start,
            last_in_text_node,
            finished,
        });
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
                Self::park(
                    &mut self.suspended,
                    &res,
                    rest,
                    next_source_location_bytes_start,
                    last_in_text_node,
                    really_last,
                );
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
                    Self::park(
                        &mut self.suspended,
                        &res,
                        raw_input.get(read..).unwrap_or_default(),
                        next_source_location_bytes_start,
                        last_in_text_node,
                        finished_decoding,
                    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AsciiCompatibleEncoding;
    use std::ops::Range;

    /// What the output handler got for one chunk: the text,
    /// `last_in_text_node` and the source range.
    type Chunk = (String, bool, Range<usize>);

    /// The parked bytes at one suspension: the address and the length of the
    /// buffer, and where the undecoded bytes start in it.
    type Parked = (*const u8, usize, usize);

    fn new_decoder() -> TextDecoder {
        TextDecoder::new(SharedEncoding::new(AsciiCompatibleEncoding::utf_8()))
    }

    /// Completes a feed whose handler may suspend. Returns the parked bytes
    /// at each suspension.
    fn resume_until_done(
        decoder: &mut TextDecoder,
        mut res: Result<(), RewritingError>,
        handler: &mut OutputHandlerCallback<'_>,
    ) -> Vec<Parked> {
        let mut parked_at = Vec::new();
        while matches!(res, Err(RewritingError::Suspended)) {
            let parked = decoder
                .take_suspended()
                .expect("a suspended feed is parked");
            parked_at.push((
                parked.remaining.as_ptr(),
                parked.remaining.len(),
                parked.offset,
            ));
            res = decoder.resume_feed(parked, handler);
        }
        res.expect("the feed completes");
        assert!(decoder.take_suspended().is_none());
        parked_at
    }

    /// Feeds `lexemes` as the lexemes of one text node, then ends the node.
    /// The handler suspends on every `every`th chunk (on none for 0).
    /// Returns the chunks and, for each lexeme, what `resume_until_done`
    /// returns.
    fn feed(lexemes: &[Vec<u8>], every: usize) -> (Vec<Chunk>, Vec<Vec<Parked>>) {
        let input_len: usize = lexemes.iter().map(Vec::len).sum();
        let mut chunks = Vec::new();
        let mut handler = |text: &str, last: bool, _: &'static Encoding, at: SourceLocation| {
            assert!(chunks.len() <= input_len, "the feed does not advance");
            chunks.push((text.to_owned(), last, at.bytes()));
            if every != 0 && chunks.len() % every == 0 {
                Err(RewritingError::Suspended)
            } else {
                Ok(())
            }
        };

        let mut decoder = new_decoder();
        let mut parked_at = Vec::new();
        let mut start = 0;
        for lexeme in lexemes {
            let res =
                decoder.feed_text(Spanned::new(start, Bytes::new(lexeme)), false, &mut handler);
            parked_at.push(resume_until_done(&mut decoder, res, &mut handler));
            start += lexeme.len();
        }
        let res = decoder.flush_pending(&mut handler);
        resume_until_done(&mut decoder, res, &mut handler);

        (chunks, parked_at)
    }

    /// ASCII text that fills `chunks` chunks of the decoder. Its period is
    /// not a multiple of the chunk length, so each chunk has its own text.
    fn ascii(chunks: usize) -> Vec<u8> {
        let period = b"abcdefghijklmnopqrstuvwxyz0123456789_";
        let len = chunks * DEFAULT_BUFFER_LEN;
        period.iter().copied().cycle().take(len).collect()
    }

    /// Text nodes whose lexemes take several chunks of the streaming decoder.
    fn text_nodes() -> Vec<Vec<Vec<u8>>> {
        let wide = "\u{20AC}\u{2192}\u{221A}\u{2603}\u{2764}\u{2600}\u{266B}".repeat(4);
        vec![
            // A byte that is not UTF-8 comes first: no fast path.
            vec![[&b"\xE9"[..], &ascii(8)].concat()],
            // The fast path takes the valid prefix, the decoder takes the rest.
            vec![[&ascii(2), &b"\xE9"[..], &ascii(8)].concat()],
            // Characters of 3 bytes do not fit the decoder's buffer evenly.
            vec![[&b"\xE9"[..], wide.as_bytes()].concat()],
            // Later lexemes of a text node always take the decoder. A
            // character is split between the last two.
            vec![
                ascii(3),
                [&ascii(5), &b"\xE2\x82"[..]].concat(),
                [&b"\xAC"[..], &ascii(5)].concat(),
            ],
        ]
    }

    #[test]
    fn suspended_feed_yields_the_same_chunks() {
        for lexemes in text_nodes() {
            let (expected, _) = feed(&lexemes, 0);
            assert!(expected.len() > 8);

            for every in [1, 2, 3, 7] {
                let (chunks, _) = feed(&lexemes, every);
                assert_eq!(chunks, expected, "every: {every}");
            }
        }
    }

    #[test]
    fn rest_of_lexeme_is_copied_once() {
        for lexemes in text_nodes() {
            let (_, parked_at) = feed(&lexemes, 1);
            let most_suspensions = parked_at.iter().map(Vec::len).max().unwrap();
            assert!(most_suspensions > 4);

            for parked_at in parked_at {
                for pair in parked_at.windows(2) {
                    let (buffer, len, offset) = pair[0];
                    let (next_buffer, next_len, next_offset) = pair[1];
                    assert_eq!(
                        (next_buffer, next_len),
                        (buffer, len),
                        "a later suspension copied or moved the rest of the lexeme"
                    );
                    assert!(next_offset > offset);
                }
            }
        }
    }

    #[test]
    fn handler_error_parks_nothing() {
        let lexeme = [&b"\xE9"[..], &ascii(8)].concat();

        for suspend_first in [false, true] {
            let mut calls = 0;
            let mut handler = |_: &str, _: bool, _: &'static Encoding, _: SourceLocation| {
                calls += 1;
                match calls {
                    1 if suspend_first => Err(RewritingError::Suspended),
                    2 => Err(RewritingError::ContentHandlerError("failed".into())),
                    _ => Ok(()),
                }
            };

            let mut decoder = new_decoder();
            let mut res =
                decoder.feed_text(Spanned::new(0, Bytes::new(&lexeme)), false, &mut handler);
            if suspend_first {
                assert!(matches!(res, Err(RewritingError::Suspended)));
                let parked = decoder.take_suspended().unwrap();
                res = decoder.resume_feed(parked, &mut handler);
            }

            assert!(matches!(res, Err(RewritingError::ContentHandlerError(_))));
            assert!(decoder.take_suspended().is_none());
        }
    }

    /// The record is inline in every rewriter, suspended or not.
    #[test]
    fn suspension_record_size() {
        assert!(size_of::<TextFeedSuspension>() <= 5 * size_of::<usize>());
    }
}
