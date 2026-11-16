//! Cargo build-script support for checking final Slang sources.

use std::env;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{SlangOrigin, SlangSource};

/// A complete shader registered for build-time checking.
pub struct Shader<'a> {
    /// Name shown in build diagnostics.
    pub name: &'a str,
    /// The final source, including all of its declarations.
    pub source: SlangSource,
    /// Directory that contains the Rust invocation assembling the shader.
    pub directory: &'a Path,
}

/// Checks each registered shader without generating shader binaries.
///
/// Call this from `build.rs` and exit with a nonzero status on error. The
/// compiler is set by `SLANGC` and defaults to `slangc` on PATH. Relative
/// directories are resolved from the Cargo package directory.
///
/// Cargo reruns the check when resolved Slang dependencies or configured
/// search directories change. The Rust modules used by the build script are
/// tracked by Cargo as part of compiling the script.
pub fn check(shaders: &[Shader<'_>], search_paths: &[&Path]) -> Result<(), String> {
    if shaders.is_empty() {
        return Ok(());
    }
    let out_dir = env::var_os("OUT_DIR").ok_or("Slang checks must run from a Cargo build script")?;
    let generated = PathBuf::from(out_dir).join("slang-inline");
    fs::create_dir_all(&generated).map_err(|error| format!("Could not create {}: {error}", generated.display()))?;
    let output_directory = generated.canonicalize().unwrap_or_else(|_| generated.clone());
    let compiler = env::var_os("SLANGC").unwrap_or_else(|| "slangc".into());
    println!("cargo::rerun-if-env-changed=SLANGC");
    println!("cargo::rerun-if-env-changed=PATH");
    if let Some(program) = compiler_path(&compiler) {
        watch(&program);
    }

    for directory in search_paths
        .iter()
        .copied()
        .chain(shaders.iter().map(|shader| shader.directory))
    {
        // Watching a directory containing OUT_DIR would also watch the files
        // this check writes. Resolved dependencies are tracked individually.
        if let Ok(directory) = directory.canonicalize()
            && !output_directory.starts_with(&directory)
        {
            watch(&directory);
        }
    }

    let mut failures = Vec::new();
    for (index, shader) in shaders.iter().enumerate() {
        let input = generated.join(format!("shader_{index}.slang"));
        if let Err(error) = check_shader(&compiler, shader, search_paths, &input) {
            failures.push(error);
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

fn compiler_path(compiler: &OsStr) -> Option<PathBuf> {
    let path = Path::new(compiler);
    if path.components().count() != 1 {
        return path.canonicalize().ok();
    }
    env::split_paths(&env::var_os("PATH")?).find_map(|directory| {
        let mut candidate = directory.join(path);
        if cfg!(windows) && path.extension().is_none() {
            candidate.set_extension("exe");
        }
        candidate.canonicalize().ok()
    })
}

fn watch(path: &Path) {
    println!("cargo::rerun-if-changed={}", path.display());
}

fn check_shader(compiler: &OsStr, shader: &Shader<'_>, search_paths: &[&Path], input: &Path) -> Result<(), String> {
    fs::write(input, shader.source.as_str())
        .map_err(|error| format!("Could not write {} for {}: {error}", input.display(), shader.name))?;
    let mut command = Command::new(compiler);
    command
        .args([
            "-no-codegen",
            "-enable-machine-readable-diagnostics",
            "-output-includes",
        ])
        .arg("-I")
        .arg(shader.directory);
    for directory in search_paths {
        command.arg("-I").arg(directory);
    }
    let output = command.arg(input).output().map_err(|error| {
        format!(
            "Could not run {:?} for {}: {error}. Set SLANGC to the Slang compiler path.",
            compiler, shader.name
        )
    })?;

    let mut details = String::new();
    for stream in [&output.stdout, &output.stderr] {
        let mut warning = false;
        for line in String::from_utf8_lossy(stream).lines() {
            let Some(diagnostic) = Diagnostic::parse(line) else {
                let _ = writeln!(details, "{line}");
                continue;
            };
            if diagnostic.kind == "note"
                && let Some(dependency) = include_path(diagnostic.message)
            {
                if dependency != input {
                    watch(dependency);
                }
                continue;
            }
            let location = diagnostic.location(shader.source, input);
            let rendered = format!("{location} [{}] {}", diagnostic.code, diagnostic.message);
            if diagnostic.kind != "span" {
                warning = diagnostic.kind == "warning";
            }
            if output.status.success() && warning {
                println!("cargo::warning={} {rendered}", shader.name);
            }
            let _ = writeln!(details, "{rendered}");
        }
    }
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "Slang check failed for {} ({}):\n{}",
            shader.name,
            output.status,
            details.trim_end()
        ))
    }
}

fn include_path(message: &str) -> Option<&Path> {
    let quoted = message.strip_prefix("include ")?.trim_start();
    Some(Path::new(quoted.strip_prefix('\'')?.strip_suffix('\'')?))
}

struct Diagnostic<'a> {
    code: &'a str,
    kind: &'a str,
    path: &'a Path,
    line: usize,
    column: usize,
    message: &'a str,
}

impl<'a> Diagnostic<'a> {
    fn parse(line: &'a str) -> Option<Self> {
        // Slang emits a tab-separated record and may follow it with span
        // records carrying more specific messages at the same location.
        let mut fields = line.splitn(8, '\t');
        Some(Self {
            code: fields.next()?,
            kind: fields.next()?,
            path: Path::new(fields.next()?),
            line: fields.next()?.parse().ok()?,
            column: fields.next()?.parse().ok()?,
            message: fields.nth(2)?,
        })
    }

    fn location(&self, source: SlangSource, input: &Path) -> String {
        let fallback = || format!("{}:{}:{}", self.path.display(), self.line, self.column);
        if self.path != input {
            return fallback();
        }
        let Some(offset) = source_offset(source.as_str(), self.line, self.column) else {
            return fallback();
        };
        for fragment in source.fragments() {
            let range = fragment.range();
            if !range.contains(&offset) && offset != range.end {
                continue;
            }
            let prefix = &source.as_str()[range.start..offset];
            let lines = prefix.bytes().filter(|&byte| byte == b'\n').count();
            let columns = prefix.rsplit('\n').next().unwrap_or_default().chars().count();
            return match fragment.origin {
                SlangOrigin::Inline { file, line, column } => {
                    let line_column = if lines == 0 { column } else { 1 };
                    format!("{file}:{}:{}", line + lines, line_column + columns)
                }
                SlangOrigin::Injected { path } => {
                    format!("Rust constant {path} (line {}, column {})", lines + 1, columns + 1)
                }
            };
        }
        fallback()
    }
}

fn source_offset(text: &str, line: usize, column: usize) -> Option<usize> {
    let preceding = line.checked_sub(1)?;
    let mut lines = text.split('\n');
    let start = lines.by_ref().take(preceding).map(|line| line.len() + 1).sum::<usize>();
    let line = lines.next()?.trim_end_matches('\r');
    let column = line
        .char_indices()
        .map(|(offset, _)| offset)
        .chain([line.len()])
        .nth(column.checked_sub(1)?)?;
    Some(start + column)
}
