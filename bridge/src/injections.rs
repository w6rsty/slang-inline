use std::collections::{HashMap, HashSet};
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use syn::ext::IdentExt;
use syn::parse::Parser;
use syn::spanned::Spanned;
use syn::{Expr, Item, ItemMod, Lit, Token};

use crate::source::{LineIndex, Position, TextRange, concat_arguments, inline_bodies, raw_string};

pub struct RustFile {
    pub uri: String,
    pub directory_uri: String,
    pub text: String,
    pub version: u64,
}

pub struct Fragment {
    pub uri: String,
    pub origin: Position,
    pub text: String,
    pub kind: FragmentKind,
}

#[derive(Clone, Copy)]
pub enum FragmentKind {
    Declaration,
    Body { raw_hashes: Option<usize> },
}

#[derive(Clone, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SourceKey {
    pub file: PathBuf,
    pub offset: usize,
}

pub struct CompositionSite {
    pub key: SourceKey,
    pub uri: String,
    pub directory_uri: String,
    pub line: u32,
    pub name: Option<String>,
    pub composition: Composition,
}

#[derive(Default)]
pub struct Composition {
    pub fragments: Vec<Fragment>,
    pub used: HashSet<SourceKey>,
}

impl Composition {
    fn append(&mut self, child: Self) {
        self.fragments.extend(child.fragments);
        self.used.extend(child.used);
    }
}

pub fn file_path(uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?;
    let decoded = percent_decode_str(encoded).decode_utf8().ok()?;
    #[cfg(windows)]
    let path = {
        let local = decoded.strip_prefix('/');
        let path = match local {
            Some(path)
                if path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
                    && path.as_bytes().get(1) == Some(&b':') =>
            {
                path[..1].to_ascii_uppercase() + &path[1..]
            }
            None => format!("//{decoded}"),
            Some(_) => return None,
        };
        PathBuf::from(path.replace('/', "\\"))
    };
    #[cfg(not(windows))]
    let path = PathBuf::from(decoded.as_ref());
    path.is_absolute().then_some(path)
}

fn path_uri(path: &Path) -> Option<String> {
    const FILE_PATH: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'/')
        .remove(b':')
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    let text = path.to_str()?;
    #[cfg(windows)]
    let text = text.replace('\\', "/");
    let prefix = if text.starts_with("//") {
        "file:"
    } else if text.starts_with('/') {
        "file://"
    } else {
        "file:///"
    };
    Some(format!("{prefix}{}", utf8_percent_encode(text.as_ref(), FILE_PATH)))
}

struct ParsedFile {
    path: PathBuf,
    uri: String,
    text: String,
    ast: syn::File,
    prefix: usize,
}

impl ParsedFile {
    fn span(&self, span: proc_macro2::Span) -> Range<usize> {
        let range = span.byte_range();
        self.prefix + range.start..self.prefix + range.end
    }

    fn composition_names(&self, items: &[Item], prefix: &str, names: &mut HashMap<usize, String>) {
        for item in items {
            match item {
                Item::Mod(module) if plain_module(module) => {
                    if let Some((_, items)) = &module.content {
                        self.composition_names(items, &format!("{prefix}{}::", module.ident), names);
                    }
                }
                Item::Const(constant) if !conditional(&constant.attrs) => {
                    let Expr::Macro(expression) = constant.expr.as_ref() else {
                        continue;
                    };
                    let span = self.span(expression.span());
                    if let Some(args) = concat_arguments(&self.text[span.clone()]).first() {
                        names.insert(span.start + args.start, format!("{prefix}{}", constant.ident));
                    }
                }
                _ => {}
            }
        }
    }
}

#[derive(Clone)]
struct Module {
    file: Rc<ParsedFile>,
    directory: PathBuf,
    inline: Vec<usize>,
}

impl Module {
    fn items(&self) -> &[Item] {
        let mut items = self.file.ast.items.as_slice();
        for &index in &self.inline {
            let Item::Mod(item) = &items[index] else { unreachable!() };
            items = &item.content.as_ref().expect("inline module").1;
        }
        items
    }

    fn nested(&self, index: usize, name: &str) -> Self {
        let mut inline = self.inline.clone();
        inline.push(index);
        Self {
            file: self.file.clone(),
            directory: self.directory.join(name),
            inline,
        }
    }

    fn at_offset(&self, offset: usize) -> Self {
        for (index, item) in self.items().iter().enumerate() {
            if let Item::Mod(item) = item
                && item.content.is_some()
                && self.file.span(item.span()).contains(&offset)
            {
                return self.nested(index, &item.ident.unraw().to_string()).at_offset(offset);
            }
        }
        self.clone()
    }
}

struct Binding {
    module: Module,
    index: usize,
}

impl Binding {
    fn constant(&self) -> &syn::ItemConst {
        let Item::Const(item) = &self.module.items()[self.index] else {
            unreachable!()
        };
        item
    }

    fn key(&self) -> SourceKey {
        let file = &self.module.file;
        SourceKey {
            file: file.path.clone(),
            offset: file.span(self.constant().span()).start,
        }
    }
}

pub struct Resolver<'a> {
    open: &'a HashMap<PathBuf, RustFile>,
    parsed: HashMap<PathBuf, Option<Rc<ParsedFile>>>,
    pub consulted: HashSet<PathBuf>,
}

impl<'a> Resolver<'a> {
    pub fn new(open: &'a HashMap<PathBuf, RustFile>) -> Self {
        Self {
            open,
            parsed: HashMap::new(),
            consulted: HashSet::new(),
        }
    }

    pub fn resolve(
        &mut self,
        host: &Path,
        source: &str,
        index: &LineIndex,
        imports: Range<usize>,
    ) -> Result<Vec<Fragment>, (TextRange, String)> {
        self.resolve_list(host, source, index, imports, |_, binding| {
            raw_constant(&binding).ok_or_else(|| "expected a raw string constant".to_owned())
        })
    }

    pub fn compose(
        &mut self,
        host: &Path,
        source: &str,
        index: &LineIndex,
        args: Range<usize>,
    ) -> Result<Composition, (TextRange, String)> {
        self.compose_list(host, source, index, args, &mut HashSet::new())
    }

    pub fn composition_sites(&mut self, host: &Path, bodies: &HashSet<SourceKey>) -> Vec<CompositionSite> {
        let mut files = HashMap::new();
        for root in crate_roots(host) {
            let Some(file) = self.load(&root) else { continue };
            let module = Module {
                file,
                directory: root.parent().unwrap_or(host).to_owned(),
                inline: Vec::new(),
            };
            if self.includes_file(&module, host) {
                self.collect_module_files(module, &mut files);
            }
        }
        // A stand-alone file or an open file outside the standard module tree
        // can still contain a composition of its own constants.
        if let Some(file) = self.load(host) {
            files.insert(host.to_owned(), file);
        }
        let mut sites = Vec::new();
        for file in files.values().filter(|file| file.text.contains("slang_concat")) {
            let Some(directory_uri) = file.path.parent().and_then(path_uri) else {
                continue;
            };
            let index = LineIndex::new(&file.text);
            let mut names = HashMap::new();
            file.composition_names(&file.ast.items, "", &mut names);
            for args in concat_arguments(&file.text) {
                let offset = args.start;
                if let Ok(composition) = self.compose(&file.path, &file.text, &index, args)
                    && !composition.used.is_disjoint(bodies)
                {
                    sites.push(CompositionSite {
                        key: SourceKey {
                            file: file.path.clone(),
                            offset,
                        },
                        uri: file.uri.clone(),
                        directory_uri: directory_uri.clone(),
                        line: index.position(&file.text, offset).line,
                        name: names.remove(&offset),
                        composition,
                    });
                }
            }
        }
        // Offer the final shaders. Intermediate concatenations contribute their
        // fragments through their parents and are checked on their own otherwise.
        let nested: HashSet<_> = sites
            .iter()
            .flat_map(|site| site.composition.used.iter().cloned())
            .collect();
        sites.retain(|site| !nested.contains(&site.key));
        sites.sort_by(|a, b| a.key.cmp(&b.key));
        sites
    }

    fn collect_module_files(&mut self, module: Module, files: &mut HashMap<PathBuf, Rc<ParsedFile>>) {
        if module.inline.is_empty() && files.insert(module.file.path.clone(), module.file.clone()).is_some() {
            return;
        }
        for (index, item) in module.items().iter().enumerate() {
            if let Item::Mod(item) = item
                && plain_module(item)
                && let Some(child) = self.child(&module, index, item)
            {
                self.collect_module_files(child, files);
            }
        }
    }

    fn compose_list(
        &mut self,
        host: &Path,
        source: &str,
        index: &LineIndex,
        args: Range<usize>,
        active: &mut HashSet<SourceKey>,
    ) -> Result<Composition, (TextRange, String)> {
        let range = TextRange {
            start: index.position(source, args.start),
            end: index.position(source, args.end),
        };
        let parts = self.resolve_list(host, source, index, args, |resolver, binding| {
            resolver.source_constant(binding, active)
        })?;
        if parts.is_empty() {
            return Err((range, "slang_concat! requires at least one source".to_owned()));
        }
        let mut combined = Composition::default();
        for part in parts {
            combined.append(part);
        }
        Ok(combined)
    }

    fn resolve_list<T>(
        &mut self,
        host: &Path,
        source: &str,
        index: &LineIndex,
        args: Range<usize>,
        mut read: impl FnMut(&mut Self, Binding) -> Result<T, String>,
    ) -> Result<Vec<T>, (TextRange, String)> {
        let text = &source[args.clone()];
        let range = |span: Range<usize>| TextRange {
            start: index.position(source, args.start + span.start),
            end: index.position(source, args.start + span.end),
        };
        let parse = |input: syn::parse::ParseStream<'_>| input.parse_terminated(syn::Path::parse_mod_style, Token![,]);
        let paths = parse
            .parse_str(text)
            .map_err(|error| (range(0..text.len()), error.to_string()))?;
        paths
            .into_iter()
            .map(|path| {
                let names: Vec<_> = path
                    .segments
                    .iter()
                    .map(|segment| segment.ident.unraw().to_string())
                    .collect();
                self.resolve_path(host, args.start, &names, path.leading_colon.is_some())
                    .map_err(str::to_owned)
                    .and_then(|binding| read(self, binding))
                    .map_err(|reason| {
                        (
                            range(path.span().byte_range()),
                            format!("{}: {reason}", names.join("::")),
                        )
                    })
            })
            .collect()
    }

    fn source_constant(&mut self, binding: Binding, active: &mut HashSet<SourceKey>) -> Result<Composition, String> {
        let key = binding.key();
        if !active.insert(key.clone()) {
            return Err("cyclic Slang source composition".to_owned());
        }
        let result = self.source_expression(&binding, active);
        active.remove(&key);
        result
    }

    fn source_expression(&mut self, binding: &Binding, active: &mut HashSet<SourceKey>) -> Result<Composition, String> {
        let Expr::Macro(expression) = binding.constant().expr.as_ref() else {
            return Err("expected a constant initialized by slang_inline! or slang_concat!".to_owned());
        };
        let file = &binding.module.file;
        let span = file.span(expression.span());
        let view = &file.text[span.clone()];
        let index = LineIndex::new(&file.text);
        let absolute = |range: Range<usize>| span.start + range.start..span.start + range.end;
        let name = expression
            .mac
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string());
        match name.as_deref() {
            Some("slang_inline") => {
                let body = inline_bodies(view)
                    .into_iter()
                    .next()
                    .ok_or("expected a closed slang_inline! block")?;
                let mut fragments = if let Some(header) = body.imports {
                    self.resolve(&file.path, &file.text, &index, absolute(header))
                        .map_err(|(_, message)| message)?
                } else {
                    Vec::new()
                };
                let body_span = absolute(body.span);
                let key = SourceKey {
                    file: file.path.clone(),
                    offset: body_span.start,
                };
                fragments.push(Fragment {
                    uri: file.uri.clone(),
                    origin: index.position(&file.text, body_span.start),
                    text: file.text[body_span].replace("\r\n", "\n"),
                    kind: FragmentKind::Body {
                        raw_hashes: body.raw_hashes,
                    },
                });
                Ok(Composition {
                    fragments,
                    used: HashSet::from([key]),
                })
            }
            Some("slang_concat") => {
                let args = concat_arguments(view)
                    .into_iter()
                    .next()
                    .ok_or("expected a closed slang_concat! call")?;
                let args = absolute(args);
                let key = SourceKey {
                    file: file.path.clone(),
                    offset: args.start,
                };
                let mut combined = self
                    .compose_list(&file.path, &file.text, &index, args, active)
                    .map_err(|(_, message)| message)?;
                combined.used.insert(key);
                Ok(combined)
            }
            _ => Err("expected slang_inline! or slang_concat!".to_owned()),
        }
    }

    fn resolve_path(
        &mut self,
        host: &Path,
        offset: usize,
        names: &[String],
        absolute: bool,
    ) -> Result<Binding, &'static str> {
        let mut alternatives = Vec::new();
        match names {
            [name] if !absolute => {
                let module = Module {
                    file: self.load(host).ok_or("could not parse the Rust file")?,
                    directory: host.parent().unwrap_or(host).to_owned(),
                    inline: Vec::new(),
                }
                .at_offset(offset);
                return constant_binding(module, name).ok_or("expected a module-level constant");
            }
            [prefix, path @ ..] if !absolute && prefix == "crate" && !path.is_empty() => {
                for root in crate_roots(host) {
                    let Some(file) = self.load(&root) else { continue };
                    let module = Module {
                        file,
                        directory: root.parent().unwrap_or(host).to_owned(),
                        inline: Vec::new(),
                    };
                    if self.includes_file(&module, host) {
                        alternatives.push(self.resolve_from(module, path));
                    }
                }
            }
            _ => return Err("use a constant name from this module or a crate:: path"),
        }
        let mut alternatives = alternatives.into_iter();
        let resolved = alternatives
            .next()
            .flatten()
            .ok_or("expected a constant in a standard Rust module")?;
        let key = resolved.key();
        if alternatives.any(|other| other.is_none_or(|binding| binding.key() != key)) {
            return Err("this Rust file belongs to crates with different definitions");
        }
        Ok(resolved)
    }

    fn resolve_from(&mut self, mut module: Module, path: &[String]) -> Option<Binding> {
        let (constant, modules) = path.split_last()?;
        for name in modules {
            let mut matches = module
                .items()
                .iter()
                .enumerate()
                .filter_map(|(index, item)| match item {
                    Item::Mod(item) if item.ident.unraw() == name && plain_module(item) => Some((index, item)),
                    _ => None,
                });
            let (index, item) = matches.next()?;
            if matches.next().is_some() {
                return None;
            }
            module = self.child(&module, index, item)?;
        }
        constant_binding(module, constant)
    }

    fn includes_file(&mut self, module: &Module, host: &Path) -> bool {
        if module.file.path == host {
            return true;
        }
        if !host.starts_with(&module.directory) {
            return false;
        }
        module.items().iter().enumerate().any(|(index, item)| {
            let Item::Mod(item) = item else { return false };
            let directory = module.directory.join(item.ident.unraw().to_string());
            plain_module(item)
                && (host.starts_with(&directory) || host == directory.with_extension("rs"))
                && self
                    .child(module, index, item)
                    .is_some_and(|child| self.includes_file(&child, host))
        })
    }

    fn child(&mut self, module: &Module, index: usize, item: &ItemMod) -> Option<Module> {
        let name = item.ident.unraw().to_string();
        if item.content.is_some() {
            return Some(module.nested(index, &name));
        }
        let directory = module.directory.join(name);
        let flat = self.load(&directory.with_extension("rs"));
        let nested = self.load(&directory.join("mod.rs"));
        let file = match (flat, nested) {
            (Some(file), None) | (None, Some(file)) => file,
            _ => return None,
        };
        Some(Module {
            file,
            directory,
            inline: Vec::new(),
        })
    }

    fn load(&mut self, path: &Path) -> Option<Rc<ParsedFile>> {
        self.consulted.insert(path.to_owned());
        self.parsed
            .entry(path.to_owned())
            .or_insert_with(|| {
                let (uri, text) = if let Some(file) = self.open.get(path) {
                    (file.uri.clone(), file.text.clone())
                } else {
                    (path_uri(path)?, fs::read_to_string(path).ok()?)
                };
                let mut rest = text.strip_prefix('\u{feff}').unwrap_or(&text);
                if let Some(shebang) = rest.strip_prefix("#!")
                    && !shebang.trim_start().starts_with('[')
                {
                    rest = &rest[rest.find('\n').unwrap_or(rest.len())..];
                }
                let ast = syn::parse_str(rest).ok()?;
                let prefix = text.len() - rest.len();
                Some(Rc::new(ParsedFile {
                    path: path.to_owned(),
                    uri,
                    text,
                    ast,
                    prefix,
                }))
            })
            .clone()
    }
}

fn constant_binding(module: Module, name: &str) -> Option<Binding> {
    let mut constants = module
        .items()
        .iter()
        .enumerate()
        .filter_map(|(index, item)| match item {
            Item::Const(item) if item.ident.unraw() == name => Some((index, item)),
            _ => None,
        });
    let (index, item) = constants.next()?;
    if constants.next().is_some() || conditional(&item.attrs) {
        return None;
    }
    Some(Binding { module, index })
}

fn raw_constant(binding: &Binding) -> Option<Fragment> {
    let Expr::Lit(expression) = binding.constant().expr.as_ref() else {
        return None;
    };
    let Lit::Str(literal) = &expression.lit else {
        return None;
    };
    let file = &binding.module.file;
    let start = file.span(literal.span()).start;
    let (_, Some(contents)) = raw_string(&file.text, start)? else {
        return None;
    };
    Some(Fragment {
        uri: file.uri.clone(),
        origin: LineIndex::new(&file.text).position(&file.text, contents.start),
        text: file.text[contents].replace("\r\n", "\n"),
        kind: FragmentKind::Declaration,
    })
}

fn conditional(attributes: &[syn::Attribute]) -> bool {
    attributes
        .iter()
        .any(|attribute| attribute.path().is_ident("cfg") || attribute.path().is_ident("cfg_attr"))
}

fn plain_module(item: &ItemMod) -> bool {
    !conditional(&item.attrs) && !item.attrs.iter().any(|attribute| attribute.path().is_ident("path"))
}

fn crate_roots(host: &Path) -> Vec<PathBuf> {
    let Some(package) = host
        .ancestors()
        .skip(1)
        .find(|directory| directory.join("Cargo.toml").is_file())
    else {
        return vec![host.to_owned()];
    };
    let Ok(relative) = host.strip_prefix(package) else {
        return Vec::new();
    };
    let parts: Vec<_> = relative.iter().collect();
    let target = |group: &Path, parts: &[&std::ffi::OsStr]| match parts {
        [file] => vec![group.join(file)],
        [name, ..] => vec![group.join(name).join("main.rs")],
        [] => Vec::new(),
    };
    match parts.as_slice() {
        [src, bin, rest @ ..] if *src == "src" && *bin == "bin" => target(&package.join("src/bin"), rest),
        [src, ..] if *src == "src" => {
            let roots = [package.join("src/lib.rs"), package.join("src/main.rs")];
            if roots.iter().any(|root| root == host) {
                vec![host.to_owned()]
            } else {
                roots.to_vec()
            }
        }
        [group, rest @ ..] if ["examples", "tests", "benches"].iter().any(|name| *group == *name) => {
            target(&package.join(group), rest)
        }
        _ => vec![host.to_owned()],
    }
}
