use proc_macro::{Delimiter, Group, Span, TokenStream, TokenTree};
use quote::quote;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::{LitStr, Path, Token};

/// Creates a static Slang source from a body and an optional `include { ... }` header.
///
/// Use bare Slang in a Rust source file, or wrap the body in a Rust raw string
/// when the Slang syntax requires tokens or delimiters that Rust cannot parse.
/// Each included path supplies a constant string and is placed before the body.
#[proc_macro]
pub fn slang_inline(source: TokenStream) -> TokenStream {
    let mut tokens = source.clone().into_iter();
    let header = match (tokens.next(), tokens.next()) {
        (Some(TokenTree::Ident(ident)), Some(TokenTree::Group(group)))
            if ident.to_string() == "include" && group.delimiter() == Delimiter::Brace =>
        {
            Some(group)
        }
        _ => None,
    };
    let (imports, tail) = if let Some(header) = &header {
        let imports = match paths(header.stream()) {
            Ok(imports) => imports,
            Err(error) => return error.into_compile_error().into(),
        };
        (imports, tokens.collect())
    } else {
        (Punctuated::new(), source)
    };

    let invocation = Span::call_site();
    let (body, file, line, column) = if let Some(body) = raw_body(tail.clone()) {
        let prefix = body.token().to_string().find('"').map_or(0, |quote| quote + 1);
        let origin = body.span().unwrap();
        (body, origin.file(), origin.line(), origin.column() + prefix)
    } else if let Some((body, line, column)) = bare_body(invocation, header.as_ref(), &tail) {
        (LitStr::new(&body, invocation.into()), invocation.file(), line, column)
    } else {
        return syn::Error::new(
            invocation.into(),
            "could not recover the original Slang text. Write the block directly in a Rust file or use a raw string",
        )
        .into_compile_error()
        .into();
    };
    let imports = imports.into_iter().map(|path| {
        let value = module_value(&path);
        let symbol = path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>()
            .join("::");
        quote!(::slang_inline::__private::Part {
            text: #value,
            origin: ::slang_inline::SlangOrigin::Injected { path: #symbol },
        })
    });
    quote!(::slang_inline::__slang_inline_parts!(
        #(#imports,)*
        ::slang_inline::__private::Part {
            text: #body,
            origin: ::slang_inline::SlangOrigin::Inline { file: #file, line: #line, column: #column },
        }
    ))
    .into()
}

/// Concatenate constant Slang sources in order and retain their fragment origins.
///
/// Each argument is a path to a `SlangSource` constant. The result is also
/// usable in a constant, with a newline separating each input.
#[proc_macro]
pub fn slang_concat(input: TokenStream) -> TokenStream {
    let sources = match paths(input) {
        Ok(paths) if !paths.is_empty() => paths,
        Ok(_) => {
            return syn::Error::new(Span::call_site().into(), "expected at least one Slang source")
                .into_compile_error()
                .into();
        }
        Err(error) => return error.into_compile_error().into(),
    };
    let values = sources.iter().map(module_value);
    quote!(::slang_inline::__slang_concat_sources!(#(#values),*)).into()
}

fn paths(stream: TokenStream) -> syn::Result<Punctuated<Path, Token![,]>> {
    let parse = |input: syn::parse::ParseStream<'_>| input.parse_terminated(Path::parse_mod_style, Token![,]);
    parse.parse(stream)
}

fn module_value(path: &Path) -> impl quote::ToTokens {
    if path.leading_colon.is_none() && path.segments.len() == 1 {
        quote!(self::#path)
    } else {
        quote!(#path)
    }
}

fn raw_body(tail: TokenStream) -> Option<LitStr> {
    let literals = Punctuated::<LitStr, Token![,]>::parse_terminated.parse(tail).ok()?;
    if literals.len() != 1 {
        return None;
    }
    let literal = literals.into_iter().next()?;
    literal.token().to_string().starts_with('r').then_some(literal)
}

fn bare_body(invocation: Span, header: Option<&Group>, tail: &TokenStream) -> Option<(String, usize, usize)> {
    let source = invocation.source_text()?;
    let (start, closing) = invocation_body(&source)?;
    let source = source.strip_suffix(closing)?;
    let body_start = if let Some(header) = header {
        let after_header = header.span_close().end();
        if invocation.file() != after_header.file() {
            return None;
        }
        let offset = relative_offset(source, invocation, after_header)?;
        if offset < start || !source.get(..offset)?.ends_with(&header.span().source_text()?) {
            return None;
        }
        offset
    } else {
        start
    };
    let (prefix, body) = source.split_at_checked(body_start)?;
    // Expanded calls can carry spans for different text. Only embed the
    // recovered source when its tokens agree with the input from rustc.
    if !same_tokens(body.parse().ok()?, tail.clone()) {
        return None;
    }
    let lines = prefix.bytes().filter(|&byte| byte == b'\n').count();
    let column_start = if lines == 0 { invocation.column() } else { 1 };
    let last_line = prefix.rsplit('\n').next().unwrap_or_default();
    Some((
        body.replace("\r\n", "\n"),
        invocation.line() + lines,
        column_start + last_line.chars().count(),
    ))
}

fn relative_offset(source: &str, invocation: Span, position: Span) -> Option<usize> {
    let row = position.line().checked_sub(invocation.line())?;
    let line_start = if row == 0 { invocation.column() } else { 1 };
    let column = position.column().checked_sub(line_start)?;
    let mut lines = source.split_inclusive('\n');
    let start = lines.by_ref().take(row).map(str::len).sum::<usize>();
    let line = lines.next().unwrap_or_default();
    let offset = line
        .char_indices()
        .map(|(offset, _)| offset)
        .chain([line.len()])
        .nth(column)?;
    Some(start + offset)
}

fn invocation_body(source: &str) -> Option<(usize, char)> {
    let mut rest = source;
    loop {
        rest = skip_trivia(rest)?;
        if let Some(after_bang) = rest.strip_prefix('!') {
            let rest = skip_trivia(after_bang)?;
            let closing = match rest.chars().next()? {
                '{' => '}',
                '(' => ')',
                '[' => ']',
                _ => return None,
            };
            return Some((source.len() - rest.len() + 1, closing));
        }
        let ch = rest.chars().next()?;
        rest = &rest[ch.len_utf8()..];
    }
}

fn skip_trivia(mut text: &str) -> Option<&str> {
    loop {
        text = text.trim_start();
        if text.starts_with("//") {
            text = text.split_once('\n').map_or("", |(_, tail)| tail);
        } else if text.starts_with("/*") {
            let mut depth = 1;
            let mut at = 2;
            let bytes = text.as_bytes();
            while depth > 0 {
                match bytes.get(at..at + 2)? {
                    b"/*" => {
                        depth += 1;
                        at += 2;
                    }
                    b"*/" => {
                        depth -= 1;
                        at += 2;
                    }
                    _ => at += 1,
                }
            }
            text = &text[at..];
        } else {
            return Some(text);
        }
    }
}

fn same_tokens(source: TokenStream, input: TokenStream) -> bool {
    let mut input = input.into_iter();
    for token in source {
        let same = match (token, input.next()) {
            (TokenTree::Group(a), Some(TokenTree::Group(b))) => {
                a.delimiter() == b.delimiter() && same_tokens(a.stream(), b.stream())
            }
            (TokenTree::Ident(a), Some(TokenTree::Ident(b))) => a.to_string() == b.to_string(),
            (TokenTree::Literal(a), Some(TokenTree::Literal(b))) => a.to_string() == b.to_string(),
            // rustc can report different joint spacing on re-lexing `#define`.
            (TokenTree::Punct(a), Some(TokenTree::Punct(b))) => a.as_char() == b.as_char(),
            _ => false,
        };
        if !same {
            return false;
        }
    }
    input.next().is_none()
}
