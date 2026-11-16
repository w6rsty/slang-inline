mod injections;
mod intelligence;
mod slang;
mod source;

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::thread;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use injections::{Composition, Fragment, FragmentKind, Resolver, RustFile, SourceKey, file_path};
use intelligence::Feature;
use slang::Slang;
use source::{LineIndex, Position, TextRange, concat_arguments, inline_bodies};

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum EditorRequest {
    Initialize {
        folders: Vec<Value>,
        #[serde(default, rename = "searchPaths")]
        search_paths: Vec<String>,
    },
    Update {
        uri: String,
        #[serde(rename = "directoryUri")]
        directory_uri: String,
        text: String,
        version: u64,
    },
    Close {
        uri: String,
    },
    RefreshDependencies {
        documents: Vec<Dependency>,
    },
    RustFilesChanged {
        uris: Vec<String>,
    },
    ListContexts {
        id: u64,
        uri: String,
        version: u64,
    },
    SelectContext {
        uri: String,
        version: u64,
        context: String,
    },
    Query(Query),
    CancelQuery {
        id: u64,
    },
}

#[derive(Deserialize)]
struct Query {
    id: u64,
    uri: String,
    version: u64,
    position: Position,
    feature: Feature,
    context: Option<Value>,
}

struct PendingQuery {
    query: Query,
    block_uri: String,
    cursor: Position,
}

#[derive(Deserialize)]
struct Dependency {
    uri: String,
    text: String,
}

enum Event {
    Editor(EditorRequest),
    EditorExit(io::Result<()>),
    Slang(u64, io::Result<Value>),
}

#[derive(Clone, Serialize, Deserialize)]
struct Diagnostic {
    range: TextRange,
    message: String,
    severity: Option<u32>,
    code: Option<Value>,
}

#[derive(Deserialize)]
struct Publication {
    uri: String,
    diagnostics: Vec<PublishedDiagnostic>,
}

#[derive(Deserialize)]
struct PublishedDiagnostic {
    #[serde(flatten)]
    diagnostic: Diagnostic,
    #[serde(default, rename = "relatedInformation")]
    related: Vec<RelatedDiagnostic>,
}

#[derive(Deserialize)]
struct RelatedDiagnostic {
    location: DiagnosticLocation,
    message: String,
}

#[derive(Deserialize)]
struct DiagnosticLocation {
    uri: String,
    range: TextRange,
}

struct Block {
    uri: String,
    directory_uri: String,
    source: String,
    regions: Vec<Region>,
    key: SourceKey,
    used: HashSet<SourceKey>,
    diagnostics: HashMap<String, Vec<Diagnostic>>,
}

struct Region {
    uri: String,
    origin: Position,
    start_line: u32,
    line_widths: Vec<u32>,
    content: Range<usize>,
    kind: FragmentKind,
}

impl Region {
    fn new(uri: String, origin: Position, start_line: u32, source: &str) -> Self {
        let line_widths = source
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line).encode_utf16().count() as u32)
            .collect();
        Self {
            uri,
            origin,
            start_line,
            line_widths,
            content: 0..source.len(),
            kind: FragmentKind::Body { raw_hashes: None },
        }
    }

    fn contains(&self, position: Position) -> bool {
        position
            .line
            .checked_sub(self.start_line)
            .and_then(|line| self.line_widths.get(line as usize))
            .is_some_and(|&width| position.character <= width)
    }

    fn position_in_rust(&self, position: Position) -> Position {
        let relative = position.line.saturating_sub(self.start_line);
        let last_line = self.line_widths.len() as u32 - 1;
        let line = relative.min(last_line);
        let width = self.line_widths[line as usize];
        let character = if relative > last_line {
            width
        } else {
            position.character.min(width)
        };
        Position {
            line: self.origin.line + line,
            character: character + if line == 0 { self.origin.character } else { 0 },
        }
    }
    fn position_in_slang(&self, rust: Position) -> Option<Position> {
        let line = rust.line.checked_sub(self.origin.line)?;
        let character = rust
            .character
            .checked_sub(if line == 0 { self.origin.character } else { 0 })?;
        let position = Position {
            line: self.start_line + line,
            character,
        };
        self.contains(position).then_some(position)
    }

    fn map_range(&self, range: TextRange) -> Option<TextRange> {
        (range.start <= range.end && self.contains(range.start) && self.contains(range.end)).then(|| TextRange {
            start: self.position_in_rust(range.start),
            end: self.position_in_rust(range.end),
        })
    }
}

impl Block {
    fn mapped_diagnostics<'a>(
        &'a self,
        host: &'a str,
        choices: &'a ContextChoices,
    ) -> impl Iterator<Item = EditorDiagnostic<'a>> + 'a {
        self.diagnostics.values().flatten().filter_map(move |diagnostic| {
            let index = self
                .regions
                .partition_point(|region| region.start_line <= diagnostic.range.start.line);
            let region = &self.regions[index.saturating_sub(1)];
            if !matches_context(choices, region, &self.key) {
                return None;
            }
            Some(EditorDiagnostic {
                uri: (region.uri != host).then_some(region.uri.as_str()),
                diagnostic: Diagnostic {
                    range: TextRange {
                        start: region.position_in_rust(diagnostic.range.start),
                        end: region.position_in_rust(diagnostic.range.end),
                    },
                    ..diagnostic.clone()
                },
            })
        })
    }
}

#[derive(Serialize)]
struct EditorDiagnostic<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    uri: Option<&'a str>,
    #[serde(flatten)]
    diagnostic: Diagnostic,
}

#[derive(Default)]
struct Document {
    version: u64,
    blocks: Vec<Block>,
    contexts: Vec<ContextOption>,
    rust_dependencies: HashSet<PathBuf>,
    injection_errors: Vec<Diagnostic>,
}

impl Document {
    fn report(
        &self,
        uri: &str,
        covered: &HashSet<SourceKey>,
        choices: &ContextChoices,
        output: &mut impl Write,
    ) -> io::Result<()> {
        let diagnostics: Vec<_> = self
            .injection_errors
            .iter()
            .cloned()
            .map(|diagnostic| EditorDiagnostic { uri: None, diagnostic })
            .chain(
                self.blocks
                    .iter()
                    .filter(|block| !covered.contains(&block.key))
                    .flat_map(|block| block.mapped_diagnostics(uri, choices)),
            )
            .collect();
        write_editor(
            output,
            &json!({
                "type": "diagnostics",
                "uri": uri,
                "version": self.version,
                "diagnostics": diagnostics
            }),
        )
    }
}

#[derive(Serialize)]
struct ContextOption {
    id: String,
    uri: String,
    line: u32,
    name: Option<String>,
}

type ContextChoices = HashMap<String, HashMap<Position, SourceKey>>;

fn matches_context(choices: &ContextChoices, region: &Region, key: &SourceKey) -> bool {
    !matches!(region.kind, FragmentKind::Body { .. })
        || choices
            .get(&region.uri)
            .and_then(|positions| positions.get(&region.origin))
            .is_none_or(|selected| selected == key)
}

#[derive(Default)]
struct Bridge {
    documents: HashMap<String, Document>,
    rust_files: HashMap<PathBuf, RustFile>,
    dependencies: HashMap<String, String>,
    next_block: u64,
    received_cross_file_diagnostics: bool,
    pending_queries: HashMap<u64, PendingQuery>,
    covered: HashSet<SourceKey>,
    context_choices: ContextChoices,
    preferred_contexts: HashMap<String, String>,
}

impl Bridge {
    fn update(&mut self, slang: &mut Slang, file: RustFile, output: &mut impl Write) -> io::Result<bool> {
        let Some(path) = file_path(&file.uri) else {
            return Ok(false);
        };
        let changed = self.rust_files.get(&path).is_none_or(|old| old.text != file.text);
        self.rust_files.insert(path.clone(), file);
        let recheck = self.rebuild(slang, &path, output)?;
        let coverage = if changed {
            self.rust_files_changed(slang, &HashSet::from([path.clone()]), Some(&path), output)?
        } else {
            self.refresh_coverage(slang, output)?
        };
        Ok(recheck || coverage)
    }

    fn rebuild(&mut self, slang: &mut Slang, path: &Path, output: &mut impl Write) -> io::Result<bool> {
        let Some(file) = self.rust_files.get(path) else {
            return Ok(false);
        };
        let RustFile {
            uri,
            directory_uri,
            text,
            version,
        } = file;
        let mut previous = self.documents.remove(uri).unwrap_or_default();
        let mut opened_blocks = false;
        let index = LineIndex::new(text);
        let mut resolver = Resolver::new(&self.rust_files);
        let mut document = Document {
            version: *version,
            ..Document::default()
        };
        let mut programs = Vec::new();
        let mut body_positions = HashMap::new();
        for body in inline_bodies(text) {
            let origin = index.position(text, body.span.start);
            let raw = &text[body.span.clone()];
            let mut fragments = if let Some(imports) = body.imports {
                match resolver.resolve(path, text, &index, imports) {
                    Ok(fragments) => fragments,
                    Err((range, message)) => {
                        document.injection_errors.push(Diagnostic {
                            range,
                            message,
                            severity: Some(1),
                            code: None,
                        });
                        continue;
                    }
                }
            } else {
                Vec::new()
            };
            fragments.push(Fragment {
                uri: uri.clone(),
                origin,
                text: raw.replace("\r\n", "\n"),
                kind: FragmentKind::Body {
                    raw_hashes: body.raw_hashes,
                },
            });
            let key = SourceKey {
                file: path.to_owned(),
                offset: body.span.start,
            };
            body_positions.insert(key.clone(), origin);
            programs.push((
                key,
                directory_uri.clone(),
                Composition {
                    fragments,
                    used: HashSet::new(),
                },
            ));
        }
        for arguments in concat_arguments(text) {
            let key = SourceKey {
                file: path.to_owned(),
                offset: arguments.start,
            };
            match resolver.compose(path, text, &index, arguments) {
                Ok(composition) => programs.push((key, directory_uri.clone(), composition)),
                Err((range, message)) => document.injection_errors.push(Diagnostic {
                    range,
                    message,
                    severity: Some(1),
                    code: None,
                }),
            }
        }
        self.context_choices.remove(uri);
        if !body_positions.is_empty() {
            let bodies = body_positions.keys().cloned().collect();
            let mut sites = resolver.composition_sites(path, &bodies);
            document.contexts = sites
                .iter()
                .map(|site| ContextOption {
                    // Use the decoded Rust path so opening a file doesn't
                    // change its ID when the editor escapes its URI differently.
                    id: match &site.name {
                        Some(name) => format!("{}#const {name}", site.key.file.display()),
                        None => format!("{}#{}", site.key.file.display(), site.key.offset),
                    },
                    uri: site.uri.clone(),
                    line: site.line,
                    name: site.name.clone(),
                })
                .collect();
            // Prefer the chosen composition wherever it contains the fragment.
            // Give other fragments in this Rust file their own default context.
            if let Some(preferred) = self.preferred_contexts.get(uri) {
                if let Some(index) = document.contexts.iter().position(|item| &item.id == preferred) {
                    sites[..=index].rotate_right(1);
                } else {
                    self.preferred_contexts.remove(uri);
                }
            }
            let mut choices = HashMap::new();
            for site in sites {
                let mut selected = false;
                for body in site.composition.used.intersection(&bodies) {
                    let position = body_positions[body];
                    if let std::collections::hash_map::Entry::Vacant(entry) = choices.entry(position) {
                        entry.insert(site.key.clone());
                        selected = true;
                    }
                }
                // Open owners already submit their own programs. Submit a
                // closed owner's shader using its directory for Slang imports.
                if selected && !self.rust_files.contains_key(&site.key.file) {
                    programs.push((site.key, site.directory_uri, site.composition));
                }
            }
            if !choices.is_empty() {
                self.context_choices.insert(uri.clone(), choices);
            }
        }
        for (key, directory_uri, Composition { fragments, used }) in programs {
            let (source, regions) = assemble(fragments);
            let block = if let Some(index) = previous
                .blocks
                .iter()
                .position(|block| block.source == source && block.directory_uri == directory_uri)
            {
                let mut block = previous.blocks.swap_remove(index);
                block.regions = regions;
                block.key = key;
                block.used = used;
                block
            } else {
                opened_blocks = true;
                Block {
                    uri: open_block(slang, &mut self.next_block, &directory_uri, &source)?,
                    directory_uri,
                    source,
                    regions,
                    key,
                    used,
                    diagnostics: HashMap::new(),
                }
            };
            document.blocks.push(block);
        }
        close_blocks(slang, previous.blocks)?;
        document.rust_dependencies = resolver.consulted;
        if !document.blocks.is_empty() || !document.injection_errors.is_empty() {
            self.documents.insert(uri.clone(), document);
        } else {
            document.report(uri, &self.covered, &self.context_choices, output)?;
        }
        Ok(opened_blocks)
    }

    fn rust_files_changed(
        &mut self,
        slang: &mut Slang,
        paths: &HashSet<PathBuf>,
        already_rebuilt: Option<&Path>,
        output: &mut impl Write,
    ) -> io::Result<bool> {
        let affected: Vec<_> = self
            .documents
            .iter()
            .filter(|(_, doc)| !doc.rust_dependencies.is_disjoint(paths))
            .filter_map(|(uri, _)| file_path(uri))
            .filter(|path| Some(path.as_path()) != already_rebuilt)
            .collect();
        let mut recheck = false;
        for path in affected {
            recheck |= self.rebuild(slang, &path, output)?;
        }
        recheck |= self.refresh_coverage(slang, output)?;
        Ok(recheck)
    }

    fn refresh_coverage(&mut self, slang: &mut Slang, output: &mut impl Write) -> io::Result<bool> {
        let covered: HashSet<_> = self
            .documents
            .values()
            .flat_map(|doc| &doc.blocks)
            .flat_map(|block| block.used.iter().cloned())
            .collect();
        let mut recheck = false;
        if covered != self.covered {
            let previous = std::mem::replace(&mut self.covered, covered);
            for document in self.documents.values_mut() {
                for block in &mut document.blocks {
                    let was_covered = previous.contains(&block.key);
                    let is_covered = self.covered.contains(&block.key);
                    if was_covered && !is_covered {
                        // slangd may have cleared diagnostics for the independent source
                        // while it was covered. Recheck it in the independent context.
                        slang.close_document(&block.uri)?;
                        block.uri = open_block(slang, &mut self.next_block, &block.directory_uri, &block.source)?;
                        block.diagnostics.clear();
                        recheck = true;
                    }
                }
            }
        }
        // A context switch can also change the diagnostics reported by other
        // open files whose shadow programs refer to the same fragment.
        for (uri, document) in &self.documents {
            document.report(uri, &self.covered, &self.context_choices, output)?;
        }
        Ok(recheck)
    }

    fn refresh_dependencies(
        &mut self,
        slang: &mut Slang,
        documents: Vec<Dependency>,
        output: &mut impl Write,
    ) -> io::Result<()> {
        let current: HashMap<_, _> = documents.into_iter().map(|doc| (doc.uri, doc.text)).collect();
        // Use overlays for dirty editor buffers. Closing an overlay exposes the
        // saved file again after a save, discard or close.
        for (uri, text) in &self.dependencies {
            if current.get(uri) != Some(text) {
                slang.close_document(uri)?;
            }
        }
        for (uri, text) in &current {
            if self.dependencies.get(uri) != Some(text) {
                slang.open_document(uri, text)?;
            }
        }
        self.dependencies = current;

        for (uri, document) in &mut self.documents {
            for block in &mut document.blocks {
                slang.close_document(&block.uri)?;
                block.uri = open_block(slang, &mut self.next_block, &block.directory_uri, &block.source)?;
                block.diagnostics.clear();
            }
            document.report(uri, &self.covered, &self.context_choices, output)?;
        }
        Ok(())
    }

    fn publish(&mut self, params: Publication, output: &mut impl Write) -> io::Result<()> {
        // slangd can canonicalize the Windows drive letter and URI escapes.
        let primary = file_name(&params.uri);
        if !primary.starts_with("__slang_inline_") && !params.diagnostics.is_empty() {
            self.received_cross_file_diagnostics = true;
        }
        let mut routed: HashMap<_, Vec<_>> = HashMap::new();
        for published in params.diagnostics {
            for related in &published.related {
                let target = file_name(&related.location.uri);
                if target != primary && target.starts_with("__slang_inline_") {
                    // Some slangd versions fold diagnostics from multiple files
                    // into a single publication using relatedInformation.
                    routed.entry(target.to_owned()).or_default().push(Diagnostic {
                        range: related.location.range,
                        message: related.message.clone(),
                        severity: published.diagnostic.severity,
                        code: None,
                    });
                }
            }
            routed.entry(primary.to_owned()).or_default().push(published.diagnostic);
        }

        for (uri, document) in &mut self.documents {
            let mut changed = false;
            for block in &mut document.blocks {
                if let Some(diagnostics) = routed.remove(file_name(&block.uri)) {
                    block.diagnostics.insert(params.uri.clone(), diagnostics);
                    changed = true;
                } else {
                    changed |= block.diagnostics.remove(&params.uri).is_some();
                }
            }
            if changed {
                document.report(uri, &self.covered, &self.context_choices, output)?;
            }
        }
        Ok(())
    }

    fn close(&mut self, slang: &mut Slang, uri: &str, output: &mut impl Write) -> io::Result<bool> {
        self.context_choices.remove(uri);
        if let Some(document) = self.documents.remove(uri) {
            close_blocks(slang, document.blocks)?;
        }
        if let Some(path) = file_path(uri)
            && self.rust_files.remove(&path).is_some()
        {
            return self.rust_files_changed(slang, &HashSet::from([path]), None, output);
        }
        Ok(false)
    }

    fn replay(&mut self, slang: &mut Slang, output: &mut impl Write) -> io::Result<()> {
        self.received_cross_file_diagnostics = false;
        for (id, _) in self.pending_queries.drain() {
            query_result(output, id, Value::Null)?;
        }
        for (uri, text) in &self.dependencies {
            slang.open_document(uri, text)?;
        }
        for (uri, document) in &mut self.documents {
            for block in &mut document.blocks {
                slang.open_document(&block.uri, &block.source)?;
                block.diagnostics.clear();
            }
            document.report(uri, &self.covered, &self.context_choices, output)?;
        }
        Ok(())
    }

    fn query(&mut self, slang: &mut Slang, query: Query, output: &mut impl Write) -> io::Result<()> {
        let Some((block, _, cursor)) = self.query_target(&query) else {
            return query_result(output, query.id, Value::Null);
        };
        let mut params = json!({ "textDocument": { "uri": block.uri }, "position": cursor });
        if let Some(context) = &query.context {
            params["context"] = context.clone();
        }
        slang.request(query.id, query.feature.method(), params)?;
        self.pending_queries.insert(
            query.id,
            PendingQuery {
                query,
                cursor,
                block_uri: block.uri.clone(),
            },
        );
        Ok(())
    }

    fn query_target(&self, query: &Query) -> Option<(&Block, &Region, Position)> {
        let file = self.rust_files.get(&file_path(&query.uri)?)?;
        if file.version != query.version {
            return None;
        }
        self.documents
            .iter()
            .flat_map(|(uri, document)| document.blocks.iter().map(move |block| (uri, block)))
            .filter(|(_, block)| !self.covered.contains(&block.key))
            .filter_map(|(owner, block)| {
                let (region, position) = block.regions.iter().find_map(|region| {
                    if region.uri == query.uri
                        && matches!(region.kind, FragmentKind::Body { .. })
                        && matches_context(&self.context_choices, region, &block.key)
                    {
                        Some((region, region.position_in_slang(query.position)?))
                    } else {
                        None
                    }
                })?;
                Some((owner, block, region, position))
            })
            .min_by_key(|(owner, block, _, _)| (block.used.is_empty(), *owner != &query.uri, *owner, block.key.offset))
            .map(|(_, block, region, position)| (block, region, position))
    }

    fn list_contexts(&self, id: u64, uri: &str, version: u64, output: &mut impl Write) -> io::Result<()> {
        let result = self
            .documents
            .get(uri)
            .filter(|document| document.version == version)
            .map_or(Value::Null, |document| {
                json!({
                    "items": document.contexts,
                    "selected": self.preferred_contexts.get(uri)
                        .or_else(|| document.contexts.first().map(|option| &option.id))
                })
            });
        query_result(output, id, result)
    }

    fn select_context(
        &mut self,
        slang: &mut Slang,
        uri: &str,
        version: u64,
        context: String,
        output: &mut impl Write,
    ) -> io::Result<bool> {
        let Some(path) = file_path(uri) else {
            return Ok(false);
        };
        if !self.documents.get(uri).is_some_and(|document| {
            document.version == version && document.contexts.iter().any(|item| item.id == context)
        }) {
            return Ok(false);
        }
        self.preferred_contexts.insert(uri.to_owned(), context);
        let rebuilt = self.rebuild(slang, &path, output)?;
        Ok(self.refresh_coverage(slang, output)? || rebuilt)
    }

    fn finish_query(&mut self, id: u64, result: Value, output: &mut impl Write) -> io::Result<()> {
        let Some(PendingQuery {
            query,
            block_uri,
            cursor,
        }) = self.pending_queries.remove(&id)
        else {
            return Ok(());
        };
        let mapped = self
            .query_target(&query)
            .filter(|(block, _, position)| block.uri == block_uri && *position == cursor)
            .map_or(Value::Null, |(block, region, _)| {
                query.feature.map_result(&block.source, region, cursor, result)
            });
        query_result(output, id, mapped)
    }
}

fn write_editor(output: &mut impl Write, message: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *output, message)?;
    writeln!(output)?;
    output.flush()
}

fn assemble(fragments: Vec<Fragment>) -> (String, Vec<Region>) {
    let mut source = String::new();
    let mut regions = Vec::with_capacity(fragments.len());
    let mut line = 0;
    for (index, fragment) in fragments.into_iter().enumerate() {
        if index != 0 {
            source.push('\n');
        }
        let mut region = Region::new(fragment.uri, fragment.origin, line, &fragment.text);
        region.kind = fragment.kind;
        region.content = source.len()..source.len() + fragment.text.len();
        regions.push(region);
        line += fragment.text.bytes().filter(|&byte| byte == b'\n').count() as u32 + 1;
        source.push_str(&fragment.text);
    }
    (source, regions)
}

fn query_result(output: &mut impl Write, id: u64, result: Value) -> io::Result<()> {
    write_editor(output, &json!({ "type": "queryResult", "id": id, "result": result }))
}

fn file_name(uri: &str) -> &str {
    uri.rsplit('/').next().unwrap_or(uri)
}

fn open_block(slang: &mut Slang, next_block: &mut u64, directory_uri: &str, source: &str) -> io::Result<String> {
    // A new URI keeps late unversioned diagnostics from superseded analyses
    // from being mistaken for diagnostics for the current source or dependencies.
    *next_block += 1;
    let uri = format!(
        "{}/__slang_inline_{}_{}.slang",
        directory_uri.trim_end_matches('/'),
        std::process::id(),
        next_block,
    );
    slang.open_document(&uri, source)?;
    Ok(uri)
}

fn close_blocks(slang: &mut Slang, blocks: Vec<Block>) -> io::Result<()> {
    for block in blocks {
        slang.close_document(&block.uri)?;
    }
    Ok(())
}

fn read_editor(events: &Sender<Event>) -> io::Result<()> {
    for line in io::stdin().lock().lines() {
        let request = serde_json::from_str(&line?)?;
        if events.send(Event::Editor(request)).is_err() {
            break;
        }
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let executable = std::env::args().nth(1).unwrap_or_else(|| "slangd".to_owned());
    let (events, inbox) = mpsc::channel();
    let editor = events.clone();
    thread::spawn(move || {
        let _ = editor.send(Event::EditorExit(read_editor(&editor)));
    });

    let (folders, search_paths) = match inbox.recv().map_err(io::Error::other)? {
        Event::Editor(EditorRequest::Initialize { folders, search_paths }) => (folders, search_paths),
        Event::EditorExit(result) => return result,
        _ => return Err(io::Error::other("expected initialization from editor")),
    };
    let mut generation = 0;
    let start = |generation| Slang::start(&executable, &folders, search_paths.clone(), events.clone(), generation);
    let mut slang = start(generation)?;
    let mut bridge = Bridge::default();
    let mut output = io::stdout().lock();
    for event in inbox {
        let recheck = match event {
            Event::Editor(EditorRequest::Update {
                uri,
                directory_uri,
                version,
                text,
            }) => bridge.update(
                &mut slang,
                RustFile {
                    uri,
                    directory_uri,
                    version,
                    text,
                },
                &mut output,
            )?,
            Event::Editor(EditorRequest::Close { uri }) => bridge.close(&mut slang, &uri, &mut output)?,
            Event::Editor(EditorRequest::RefreshDependencies { documents }) => {
                bridge.refresh_dependencies(&mut slang, documents, &mut output)?;
                !bridge.documents.is_empty()
            }
            Event::Editor(EditorRequest::RustFilesChanged { uris }) => {
                let paths = uris.iter().filter_map(|uri| file_path(uri)).collect();
                bridge.rust_files_changed(&mut slang, &paths, None, &mut output)?
            }
            Event::Editor(EditorRequest::ListContexts { id, uri, version }) => {
                bridge.list_contexts(id, &uri, version, &mut output)?;
                false
            }
            Event::Editor(EditorRequest::SelectContext { uri, version, context }) => {
                bridge.select_context(&mut slang, &uri, version, context, &mut output)?
            }
            Event::Editor(EditorRequest::Query(query)) => {
                bridge.query(&mut slang, query, &mut output)?;
                false
            }
            Event::Editor(EditorRequest::CancelQuery { id }) => {
                if bridge.pending_queries.remove(&id).is_some() {
                    slang.notify("$/cancelRequest", json!({ "id": id }))?;
                }
                false
            }
            Event::Editor(EditorRequest::Initialize { .. }) => {
                return Err(io::Error::other("editor already initialized"));
            }
            Event::EditorExit(result) => return result,
            Event::Slang(sender_generation, result) if sender_generation == generation => {
                let message = result?;
                if let Some(id) = message.get("id") {
                    if message.get("method").is_some() {
                        slang.respond(&message, id)?;
                    } else if let Some(id) = id.as_u64() {
                        bridge.finish_query(id, message["result"].clone(), &mut output)?;
                    }
                } else if message["method"] == "textDocument/publishDiagnostics" {
                    bridge.publish(serde_json::from_value(message["params"].clone())?, &mut output)?;
                }
                false
            }
            Event::Slang(..) => false,
        };
        if recheck && bridge.received_cross_file_diagnostics {
            // slangd can cache a publication under a dependency path and then
            // suppress changed related locations when a virtual document reopens.
            generation += 1;
            slang = start(generation)?;
            bridge.replay(&mut slang, &mut output)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: &str = r#"// 🦀
struct A { float roughness; };"#;
    const SECOND: &str = r#"struct B { A material; };"#;

    const PROOF_LIBRARY: slang_inline::SlangSource = slang_inline::slang_inline! {
        include { FIRST }
        r#"// additional library text 🦀"#
    };
    const PROOF_SURFACE: slang_inline::SlangSource = slang_inline:: /* ! { 🦀 } */ slang_inline! {
        // The original comment and the directive line are part of the static source.
        #include "surface.slangh"
        #define INJECT_FREE_VALUE 1.0
        float f(A value) { return value.roughness * INJECT_FREE_VALUE; }
    };
    const PROOF_ADDON: slang_inline::SlangSource =
        slang_inline::slang_inline!(r#"float g(A value) { return f(value); }"#,);
    const PROOF_JOIN: slang_inline::SlangSource = slang_inline::slang_concat!(PROOF_LIBRARY, PROOF_SURFACE);
    const PROOF_NESTED: slang_inline::SlangSource = slang_inline::slang_concat!(PROOF_JOIN, PROOF_ADDON);
    const PROOF_TEXT: &str = PROOF_NESTED.as_str();

    #[test]
    fn nested_composition_has_exact_static_text_and_flat_fragment_ranges() {
        assert_eq!(
            PROOF_TEXT,
            format!(
                "{}\n{}\n{}",
                PROOF_LIBRARY.as_str(),
                PROOF_SURFACE.as_str(),
                PROOF_ADDON.as_str()
            ),
        );
        let ranges = PROOF_NESTED.fragments();
        assert_eq!(ranges.len(), 4);
        assert_eq!(ranges[0].origin, slang_inline::SlangOrigin::Injected { path: "FIRST" });
        assert_eq!(&PROOF_TEXT[ranges[0].range()], FIRST);
        assert_eq!(&PROOF_TEXT[ranges[2].range()], PROOF_SURFACE.as_str());
        assert!(PROOF_SURFACE.as_str().contains("// The original comment"));
        assert!(PROOF_SURFACE.as_str().contains("#include \"surface.slangh\""));
        assert_eq!(&PROOF_TEXT[ranges[3].range()], PROOF_ADDON.as_str());
        assert!(
            matches!(ranges[3].origin, slang_inline::SlangOrigin::Inline { file, line, column }
            if file.ends_with("main.rs") && line > 0 && column > 0)
        );

        let editor = include_str!("main.rs").replace("\r\n", "\n").replace('\n', "\r\n");
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
        let mut open = HashMap::new();
        open.insert(
            path.clone(),
            RustFile {
                uri: "file:///test.rs".to_owned(),
                directory_uri: String::new(),
                text: editor.clone(),
                version: 1,
            },
        );
        let arguments = concat_arguments(&editor)
            .into_iter()
            .find(|args| {
                editor[args.clone()]
                    .split_whitespace()
                    .collect::<String>()
                    .trim_end_matches(',')
                    == "PROOF_JOIN,PROOF_ADDON"
            })
            .unwrap();
        let combined = Resolver::new(&open)
            .compose(&path, &editor, &LineIndex::new(&editor), arguments)
            .unwrap();
        assert_eq!(combined.used.len(), 4);
        let (text, regions) = assemble(combined.fragments);
        assert_eq!(text.as_bytes(), PROOF_TEXT.as_bytes());
        assert_eq!(
            regions.iter().map(|region| region.content.clone()).collect::<Vec<_>>(),
            ranges
                .iter()
                .map(slang_inline::SlangFragment::range)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn assembles_the_same_bytes_as_the_rust_expression() {
        let compiled = slang_inline::slang_inline! {
            include { FIRST, SECOND, }
            r#"
float f(B value) { return value.material.roughness; }
"#
        };
        let editor = r##"
const FIRST: &str = r#"// 🦀
struct A { float roughness; };"#;
const SECOND: &str = r#"struct B { A material; };"#;
fn shader() -> SlangSource {
    slang_inline! {
        include { FIRST, SECOND, }
        r#"
float f(B value) { return value.material.roughness; }
"#
    }
}
"##
        .replace('\n', "\r\n");
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test.rs");
        let uri = "file:///test.rs";
        let mut open = HashMap::new();
        open.insert(
            path.clone(),
            RustFile {
                uri: uri.to_owned(),
                directory_uri: String::new(),
                text: editor.clone(),
                version: 1,
            },
        );
        let body = inline_bodies(&editor).remove(0);
        let index = LineIndex::new(&editor);
        let origin = index.position(&editor, body.span.start);
        let mut fragments = Resolver::new(&open)
            .resolve(&path, &editor, &index, body.imports.unwrap())
            .unwrap();
        fragments.push(Fragment {
            uri: uri.to_owned(),
            origin,
            text: editor[body.span].replace("\r\n", "\n"),
            kind: FragmentKind::Body {
                raw_hashes: body.raw_hashes,
            },
        });
        let (source, regions) = assemble(fragments);
        assert_eq!(source.as_bytes(), compiled.as_str().as_bytes());
        assert_eq!(regions[0].origin.line, 1);
        assert_eq!(regions[2].origin, origin);
    }

    #[test]
    fn bare_injection_keeps_the_same_source_in_rust_and_the_editor() {
        let multiline = slang_inline::slang_inline! {
            include { FIRST, SECOND }
            // A comment between directives must remain on its own line.
            #define SLANG_INLINE_VALUE 42
            float shade(B value) { return value.material.roughness * SLANG_INLINE_VALUE; }
        };
        let same_line = slang_inline::slang_inline! { include { /* 🦀 */ FIRST } /* preserve inline comment */ float one() { return 1; } };
        let editor = include_str!("main.rs").replace("\r\n", "\n").replace('\n', "\r\n");
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
        let uri = "file:///test.rs";
        let index = LineIndex::new(&editor);
        let mut open = HashMap::new();
        open.insert(
            path.clone(),
            RustFile {
                uri: uri.to_owned(),
                directory_uri: String::new(),
                text: editor.clone(),
                version: 1,
            },
        );
        let mut resolver = Resolver::new(&open);
        let bodies = inline_bodies(&editor);
        for (compiled, marker) in [
            (multiline, "#define SLANG_INLINE_VALUE 42"),
            (same_line, "/* preserve inline comment */"),
        ] {
            let body = bodies
                .iter()
                .find(|body| {
                    body.raw_hashes.is_none() && body.imports.is_some() && editor[body.span.clone()].contains(marker)
                })
                .unwrap();
            let mut fragments = resolver
                .resolve(&path, &editor, &index, body.imports.clone().unwrap())
                .unwrap();
            fragments.push(Fragment {
                uri: uri.to_owned(),
                origin: index.position(&editor, body.span.start),
                text: editor[body.span.clone()].replace("\r\n", "\n"),
                kind: FragmentKind::Body {
                    raw_hashes: body.raw_hashes,
                },
            });
            let (assembled, _) = assemble(fragments);
            assert_eq!(assembled.as_bytes(), compiled.as_str().as_bytes());
            assert!(compiled.as_str().contains(marker));
        }
    }

    #[test]
    fn routes_and_clears_related_diagnostics_from_dependencies() -> io::Result<()> {
        let rust = "file:///src/main.rs";
        let shadow = "file:///src/__slang_inline_1_2.slang";
        let header = "file:///shaders/shared.slang";
        let mut bridge = Bridge::default();
        bridge.documents.insert(
            rust.to_owned(),
            Document {
                version: 1,
                blocks: vec![Block {
                    uri: shadow.to_owned(),
                    directory_uri: "file:///src".to_owned(),
                    source: "sample".to_owned(),
                    key: SourceKey::default(),
                    used: HashSet::new(),
                    regions: vec![Region::new(
                        rust.to_owned(),
                        Position { line: 4, character: 15 },
                        0,
                        "sample",
                    )],
                    diagnostics: HashMap::new(),
                }],
                ..Document::default()
            },
        );

        let range = json!({
            "start": { "line": 0, "character": 0 },
            "end": { "line": 0, "character": 6 }
        });
        let publication = serde_json::from_value(json!({
            "uri": header,
            "diagnostics": [{
                "range": range,
                "message": "Dependency diagnostic",
                "severity": 1,
                "code": 30015,
                "relatedInformation": [{
                    "location": { "uri": shadow, "range": range },
                    "message": "Diagnostic in the inline block"
                }]
            }]
        }))?;
        let mut output = Vec::new();
        bridge.publish(publication, &mut output)?;
        assert!(bridge.received_cross_file_diagnostics);
        let notification: Value = serde_json::from_slice(&output)?;
        assert_eq!(notification["uri"], rust);
        assert_eq!(notification["diagnostics"].as_array().unwrap().len(), 1);
        let diagnostic = &notification["diagnostics"][0];
        assert_eq!(diagnostic["message"], "Diagnostic in the inline block");
        assert_eq!(diagnostic["range"]["start"], json!({ "line": 4, "character": 15 }));
        assert_eq!(diagnostic["severity"], 1);
        assert!(diagnostic["code"].is_null());

        output.clear();
        bridge.publish(
            Publication {
                uri: header.to_owned(),
                diagnostics: Vec::new(),
            },
            &mut output,
        )?;
        let notification: Value = serde_json::from_slice(&output)?;
        assert_eq!(notification["diagnostics"], json!([]));
        Ok(())
    }
}
