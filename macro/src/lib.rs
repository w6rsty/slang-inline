//! Const-capable Slang source fragments for use in Rust.
//!
//! The macros produce text and source metadata. The VS Code extension runs
//! Slang analysis in the editor.

use std::ops::Range;

pub use slang_inline_macros::{slang_concat, slang_inline};

/// Static Slang text and the provenance of each fragment that contributed to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlangSource {
    text: &'static str,
    fragments: &'static [SlangFragment],
}

impl SlangSource {
    /// Returns the complete source as a contiguous static string.
    pub const fn as_str(&self) -> &'static str {
        self.text
    }

    /// Returns fragments in source order, including those from nested concatenations.
    pub const fn fragments(&self) -> &'static [SlangFragment] {
        self.fragments
    }
}

impl AsRef<str> for SlangSource {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// The location and origin of one span of text in a [`SlangSource`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlangFragment {
    start: usize,
    end: usize,
    /// The Rust source location or constant that supplied this text.
    pub origin: SlangOrigin,
}

impl SlangFragment {
    /// Byte offsets in the complete source, excluding the separator newline.
    pub const fn range(&self) -> Range<usize> {
        self.start..self.end
    }
}

/// Describes where a fragment was written or included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlangOrigin {
    /// Slang written in the body of a `slang_inline!` invocation.
    Inline {
        /// The Rust source file reported by the compiler.
        file: &'static str,
        /// One-based position in the Rust source.
        line: usize,
        /// One-based column in the Rust source.
        column: usize,
    },
    /// Slang included from a Rust string constant.
    Injected {
        /// The constant path as written in the `include` header.
        path: &'static str,
    },
}

#[doc(hidden)]
#[macro_export]
macro_rules! __slang_concat_sources {
    ($($source:expr),+ $(,)?) => {{
        const SOURCES: &[$crate::SlangSource] = &[$($source),+];
        const COUNT: usize = $crate::__private::fragment_count(SOURCES);
        const FRAGMENTS: [$crate::SlangFragment; COUNT] = $crate::__private::flatten(SOURCES);
        const TEXTS: [&str; SOURCES.len()] = $crate::__private::source_texts(SOURCES);
        $crate::__slang_make_source!(TEXTS, FRAGMENTS)
    }};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __slang_inline_parts {
    ($($part:expr),+ $(,)?) => {{
        const PARTS: &[$crate::__private::Part] = &[$($part),+];
        const TEXTS: [&str; PARTS.len()] = $crate::__private::part_texts(PARTS);
        const FRAGMENTS: [$crate::SlangFragment; PARTS.len()] = $crate::__private::part_fragments(PARTS);
        $crate::__slang_make_source!(TEXTS, FRAGMENTS)
    }};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __slang_make_source {
    ($texts:ident, $fragments:ident) => {{
        const LEN: usize = $crate::__private::joined_len(&$texts);
        const BYTES: [u8; LEN] = $crate::__private::join_bytes(&$texts);
        const TEXT: &str = $crate::__private::as_utf8(&BYTES);
        $crate::__private::source(TEXT, &$fragments)
    }};
}

#[doc(hidden)]
pub mod __private {
    use super::{SlangFragment, SlangOrigin, SlangSource};

    const EMPTY: SlangFragment = SlangFragment {
        start: 0,
        end: 0,
        origin: SlangOrigin::Injected { path: "" },
    };

    pub struct Part {
        pub text: &'static str,
        pub origin: SlangOrigin,
    }

    pub const fn source(text: &'static str, fragments: &'static [SlangFragment]) -> SlangSource {
        SlangSource { text, fragments }
    }

    pub const fn joined_len(parts: &[&str]) -> usize {
        let mut size = parts.len().saturating_sub(1);
        let mut index = 0;
        while index < parts.len() {
            size += parts[index].len();
            index += 1;
        }
        size
    }

    pub const fn join_bytes<const N: usize>(parts: &[&str]) -> [u8; N] {
        let mut result = [0; N];
        let mut offset = 0;
        let mut index = 0;
        while index < parts.len() {
            if index != 0 {
                result[offset] = b'\n';
                offset += 1;
            }
            let bytes = parts[index].as_bytes();
            let mut byte = 0;
            while byte < bytes.len() {
                result[offset] = bytes[byte];
                offset += 1;
                byte += 1;
            }
            index += 1;
        }
        result
    }

    pub const fn as_utf8(bytes: &[u8]) -> &str {
        match std::str::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => panic!("Slang source is not UTF-8"),
        }
    }

    pub const fn part_texts<const N: usize>(parts: &[Part]) -> [&'static str; N] {
        let mut result = [""; N];
        let mut index = 0;
        while index < N {
            result[index] = parts[index].text;
            index += 1;
        }
        result
    }

    pub const fn part_fragments<const N: usize>(parts: &[Part]) -> [SlangFragment; N] {
        let mut result = [EMPTY; N];
        let mut offset = 0;
        let mut index = 0;
        while index < N {
            let part = &parts[index];
            let end = offset + part.text.len();
            result[index] = SlangFragment {
                start: offset,
                end,
                origin: part.origin,
            };
            offset = end + 1;
            index += 1;
        }
        result
    }

    pub const fn source_texts<const N: usize>(sources: &[SlangSource]) -> [&'static str; N] {
        let mut result = [""; N];
        let mut index = 0;
        while index < N {
            result[index] = sources[index].text;
            index += 1;
        }
        result
    }

    pub const fn fragment_count(sources: &[SlangSource]) -> usize {
        let mut count = 0;
        let mut index = 0;
        while index < sources.len() {
            count += sources[index].fragments.len();
            index += 1;
        }
        count
    }

    pub const fn flatten<const N: usize>(sources: &[SlangSource]) -> [SlangFragment; N] {
        let mut result = [EMPTY; N];
        let mut offset = 0;
        let mut next = 0;
        let mut index = 0;
        while index < sources.len() {
            let source = &sources[index];
            let mut fragment = 0;
            while fragment < source.fragments.len() {
                let entry = source.fragments[fragment];
                result[next] = SlangFragment {
                    start: offset + entry.start,
                    end: offset + entry.end,
                    origin: entry.origin,
                };
                fragment += 1;
                next += 1;
            }
            offset += source.text.len() + 1;
            index += 1;
        }
        result
    }
}
