use std::any::Any;

pub(crate) use self::mutations::{Mutations, StringChunk};
pub(crate) use self::text_decoder::{TextDecoder, TextSink};
pub(crate) use self::text_encoder::{IncompleteUtf8Resync, TextEncoder};

pub use self::document_end::*;
pub use self::element::*;
pub use self::mutations::{ContentType, StreamingHandler};
pub use self::streaming_sink::StreamingHandlerSink;
pub use self::text_encoder::Utf8Error;
pub use self::tokens::*;

/// Data that can be attached to a rewritable unit by a user and shared between content handler
/// invocations.
///
/// Same rewritable units can be passed to different content handlers if all of them capture the
/// unit. `UserData` trait provides capability to attach arbitrary data to a rewritable unit, so
/// handlers can make decision on how to process the unit based on the information provided by
/// previous handlers.
///
/// # Example
/// ```
/// use lol_html::{rewrite_str, element, RewriteStrSettings};
/// use lol_html::html_content::UserData;
///
/// rewrite_str(
///     r#"<div id="foo"></div>"#,
///     RewriteStrSettings {
///         element_content_handlers: vec![
///             element!("*", |el| {
///                 el.set_user_data("Captured by `*`");
///
///                 Ok(())
///             }),
///             element!("#foo", |el| {
///                 let user_data = el.user_data_mut().downcast_mut::<&'static str>().unwrap();
///
///                 assert_eq!(*user_data, "Captured by `*`");
///
///                 *user_data = "Captured by `#foo`";
///
///                 Ok(())
///             }),
///             element!("div", |el| {
///                 let user_data = el.user_data().downcast_ref::<&'static str>().unwrap();
///
///                 assert_eq!(*user_data, "Captured by `#foo`");
///
///                 Ok(())
///             })
///         ],
///         ..RewriteStrSettings::new()
///     }
/// ).unwrap();
/// ```
pub trait UserData {
    /// Returns a reference to the attached user data.
    fn user_data(&self) -> &dyn Any;
    /// Returns a mutable reference to the attached user data.
    fn user_data_mut(&mut self) -> &mut dyn Any;
    /// Attaches user data to a rewritable unit.
    ///
    /// The data must be [`Send`] so that a rewritable unit detached by a
    /// handler suspension doesn't strip [`Send`] from a `send::HtmlRewriter`.
    fn set_user_data(&mut self, data: impl Any + Send);
}

macro_rules! impl_user_data {
    ($Unit:ident<$($lt:lifetime),+>) => {
        impl crate::rewritable_units::UserData for $Unit<$($lt),+> {
            #[inline]
            fn user_data(&self) -> &dyn Any {
                &*self.user_data
            }

            #[inline]
            fn user_data_mut(&mut self) -> &mut dyn Any {
                &mut *self.user_data
            }

            #[inline]
            fn set_user_data(&mut self, data: impl Any + Send){
                self.user_data = Box::new(data);
            }
        }
    };
}

#[macro_use]
mod mutations;

mod document_end;
mod element;
mod streaming_sink;
mod text_decoder;
mod text_encoder;
mod tokens;

#[cfg(test)]
mod test_utils {
    use crate::rewriter::AsciiCompatibleEncoding;
    use crate::test_utils::{ASCII_COMPATIBLE_ENCODINGS, Output};
    use crate::*;
    use encoding_rs::Encoding;
    use std::borrow::Cow;
    use std::cell::RefCell;
    use std::rc::Rc;

    pub(crate) fn encoded(input: &str) -> Vec<(Vec<u8>, &'static Encoding)> {
        ASCII_COMPATIBLE_ENCODINGS
            .iter()
            .filter_map(|enc| {
                let (input, _, has_unmappable_characters) = enc.encode(input);

                // NOTE: there is no character in existence outside of ASCII range
                // that can be represented in all the ASCII-compatible encodings.
                // So, if test cases contains some non-ASCII characters that can't
                // be represented in the given encoding then we just skip it.
                // It is OK to do so, because our intention is not to test the
                // encoding library itself (it is already well tested), but test
                // how our own code works with non-ASCII characters.
                if has_unmappable_characters {
                    None
                } else {
                    Some((input.into_owned(), *enc))
                }
            })
            .collect()
    }

    pub(crate) fn rewrite_html<'h>(
        html: &[u8],
        encoding: &'static Encoding,
        element_content_handlers: Vec<(Cow<'_, Selector>, ElementContentHandlers<'h>)>,
        document_content_handlers: Vec<DocumentContentHandlers<'h>>,
    ) -> String {
        let mut output = Output::new(encoding);

        {
            let mut rewriter = HtmlRewriter::new(
                Settings {
                    element_content_handlers,
                    document_content_handlers,
                    encoding: AsciiCompatibleEncoding::new(encoding).unwrap(),
                    ..Settings::new()
                },
                |c: &[u8]| output.push(c),
            );

            for ch in html.chunks(15) {
                rewriter.write(ch).unwrap();
            }
            rewriter.end().unwrap();
        }

        output.into()
    }

    /// xorshift64: the tests below have to be deterministic.
    pub(crate) struct Rng(pub u64);

    impl Rng {
        pub fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        pub fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// Appends up to `len` bytes of text: ASCII, characters of every UTF-8
    /// length, markup-like bytes and, with `damage`, malformed sequences.
    pub(crate) fn random_text(rng: &mut Rng, len: usize, damage: bool, out: &mut Vec<u8>) {
        const PIECES: [&str; 12] = [
            "a",
            "b",
            " ",
            "&amp;",
            "\u{e9}",
            "\u{416}",
            "\u{20ac}",
            "\u{65e5}",
            "\u{1F600}",
            "\u{10FFFF}",
            "\u{fffd}",
            "x",
        ];
        let end = out.len() + len;
        while out.len() < end {
            let piece = PIECES[rng.below(PIECES.len() as u64) as usize].as_bytes();
            if damage && rng.below(5) == 0 {
                match rng.below(7) {
                    // a lone continuation byte, or a byte that is never valid
                    0 => out.push(0x80 + rng.below(0x80) as u8),
                    // a character without its last byte
                    1 => {
                        out.extend_from_slice(&piece[..piece.len() - usize::from(piece.len() > 1)])
                    }
                    // a lead byte whose second byte is out of its range
                    2 => out.extend_from_slice(&[0xE0, 0x80][..1 + rng.below(2) as usize]),
                    3 => out.extend_from_slice(&[0xED, 0xA0, 0x80][..1 + rng.below(3) as usize]),
                    4 => out.extend_from_slice(&[0xF4, 0x90, 0x80][..1 + rng.below(3) as usize]),
                    5 => out.extend_from_slice(&[0xF0, 0x9F, 0x98][..1 + rng.below(3) as usize]),
                    // a legacy single-byte character
                    _ => out.push(0xE9),
                }
            } else {
                out.extend_from_slice(piece);
            }
        }
    }

    /// Cuts `0..len` into consecutive non-empty ranges, given as their ends.
    pub(crate) fn random_cuts(rng: &mut Rng, len: usize) -> Vec<usize> {
        let max_step = match rng.below(4) {
            0 => usize::MAX,
            1 => 9,
            2 => 3,
            _ => 40,
        };
        let mut cuts = vec![];
        let mut at = 0;
        while at < len {
            let step = if max_step == usize::MAX {
                len
            } else {
                1 + rng.below(max_step as u64) as usize
            };
            at = (at + step).min(len);
            cuts.push(at);
        }
        cuts
    }

    pub(crate) type TestSink = Box<dyn FnMut(&[u8])>;

    /// Feeds `writes` to a rewriter one `write()` each, then ends it. After
    /// every suspension it calls `on_suspend` and resumes. Returns the output
    /// bytes and the number of suspensions.
    pub(crate) fn rewrite_writes<'h>(
        writes: &[&[u8]],
        settings: Settings<'h, '_>,
        mut on_suspend: impl FnMut(&mut HtmlRewriter<'h, TestSink>),
    ) -> (Vec<u8>, usize) {
        let out = Rc::new(RefCell::new(Vec::<u8>::new()));
        let out_in_sink = Rc::clone(&out);
        let sink: TestSink = Box::new(move |c: &[u8]| {
            out_in_sink.borrow_mut().extend_from_slice(c);
        });

        let mut rewriter = HtmlRewriter::new(settings, sink);
        let mut suspensions = 0;

        let mut pump = |rewriter: &mut HtmlRewriter<'h, TestSink>,
                        mut res: Result<(), errors::RewritingError>| {
            loop {
                match res {
                    Ok(()) => return,
                    Err(errors::RewritingError::Suspended) => {
                        assert!(rewriter.is_suspended());
                        suspensions += 1;
                        on_suspend(rewriter);
                        res = rewriter.resume();
                    }
                    Err(e) => panic!("unexpected rewriting error: {e}"),
                }
            }
        };

        for write in writes {
            let res = rewriter.write(write);
            pump(&mut rewriter, res);
        }
        let res = rewriter.end_mut();
        pump(&mut rewriter, res);

        assert!(!rewriter.is_suspended());
        drop(rewriter);

        (Rc::try_unwrap(out).unwrap().into_inner(), suspensions)
    }
}
