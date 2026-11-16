# slang-inline

Write Slang inside Rust and get diagnostics, completion, and hover from `slangd` in VS Code. The Rust macros produce static source text and fragment metadata. Slang analysis runs in the editor.

```rust
use slang_inline::{SlangSource, slang_inline};

const SCALE: SlangSource = slang_inline! {
    float scale(float x) { return x * 2.0; }
};
const SCALE_TEXT: &'static str = SCALE.as_str();
```

Use `const _: SlangSource = slang_inline! { … };` for blocks you only want to check in the editor. Add the Rust crate as a path dependency pointing at the repository's `macro` directory.

```toml
[dependencies]
slang-inline = { path = "../slang-inline/macro" }
```

## Install the Windows extension

The VSIX targets Windows x64 and includes the Rust bridge. Use VS Code 1.85 or newer and make `slangd.exe` available on `PATH`, for example through the Vulkan SDK. You can also set `slangdInline.slangdPath` to its absolute path in your VS Code user settings.

Build the package from the repository root with PowerShell and the `x86_64-pc-windows-msvc` Rust toolchain. The script builds the release bridge and writes the VSIX to `target/vsix`. Use `-Offline` when the Cargo dependencies are already cached.

```powershell
.\scripts\package-vsix.ps1
code --install-extension .\target\vsix\slang-inline-0.1.0-win32-x64.vsix
```

You can also choose **Extensions: Install from VSIX...** in the Command Palette. Bridge and language server output appears in the **Slang Inline Diagnostics** channel. `slangdInline.bridgePath` can override the bundled bridge.

## Include Rust declarations

Use the optional `include { ... }` header to put one or more Rust string constants before the Slang body. The editor resolves module-level constants initialized directly with a Rust raw string. Reference a constant by name in its own module or with a `crate::` path in the same Cargo target.

```rust
use slang_inline::{SlangSource, slang_inline};

const MATERIAL_DECLS: &str = r#"
struct Material {
    float roughness;
};
"#;

const SURFACE: SlangSource = slang_inline! {
    include { MATERIAL_DECLS }

    float shade(Material material) {
        return material.roughness;
    }
};
```

The declarations are joined in header order with one separator newline between fragments. Unsaved edits to a declaration update analysis, and declaration diagnostics point back to the source constant. Slang file dependencies use `import` or `#include "file.slangh"` inside the Slang body.

Bare bodies retain their comments and line breaks. Write the invocation directly in a Rust file and use Rust-compatible tokens and balanced delimiters. Use a raw body such as `slang_inline! { r#"..."# }` when preprocessor branches need to balance delimiters separately. A raw body can also follow an `include` header.

The editor bridge reads the open Rust buffer directly. If rust-analyzer expands a bare block without access to the original text, it can still infer the result type. During a Cargo build, the macro requires the exact text for bare blocks. Use a raw body if a generated call fails with `could not recover the original Slang text`.

## Compose static sources

`slang_concat!` joins `SlangSource` constants in order with one newline between inputs. Nested compositions preserve the original fragments. `as_str()` exposes the contiguous `&'static str` and `fragments()` exposes each fragment's byte range and origin. Inline origins carry a Rust source location, while included strings record the Rust constant path.

```rust
const SHADER: SlangSource = slang_concat!(LIBRARY, crate::surface::SURFACE);
const SHADER_TEXT: &'static str = SHADER.as_str();
```

Open a fragment's Rust file to analyze it with its assembled Slang declarations. The bridge follows ordinary Rust module declarations from the fragment's conventional Cargo target roots and reads closed composition files from disk. Open Rust buffers take precedence. Compositions are discovered from calls the bridge can resolve, and nested compositions are analyzed in their final shader. Without a discovered composition, the fragment is analyzed independently.

When a fragment belongs to multiple shaders, run **Slang Inline: Select Composition** in the Command Palette. Entries show the constant name when available, along with the Rust file and line of the composition. The default is the first matching final composition in file-and-source order. A selection applies to the matching fragments in the active file for the current bridge session.

For a working example, open only `macro/examples/injected_shader/surface.rs`. Completion after `material.` shows `roughness` and `metallic` in the default composition. Select the second composition in `main.rs` to see `specular` as well. Run the example to print the assembled source. Pass `-- --detail` for the second shader.

```powershell
cargo run --locked -p slang-inline --example injected_shader
```

## Configure Slang file dependencies

Use `slangdInline.searchPaths` for Slang imports and includes. Relative paths resolve from each VS Code workspace folder.

```json
{
  "slangdInline.searchPaths": ["shaders", "../shared/shaders"]
}
```

Virtual Slang documents resolve relative file dependencies from the directory containing the Rust macro invocation. Composed shaders use their `slang_concat!` invocation's directory. Open dirty `.slang`, `.slangh`, `.hlsl`, `.hlsli`, `.h`, and `.inc` files are sent to `slangd` as overlays. Changes to these files under watched workspace and search directories trigger reanalysis.

## Develop and test

Build the debug bridge and launch a VS Code Extension Development Host.

```powershell
cargo build --locked -p slang-inline-bridge
code --extensionDevelopmentPath="$PWD/extension" "$PWD"
```

Open `macro/examples/diagnostics.rs`. Fix the missing semicolon in `broken` to clear the diagnostic without saving. In `sample`, trigger completion after `image.` or hover over `image`. The intentionally broken Slang example still passes Rust's `cargo check`.

```powershell
cargo check --locked -p slang-inline --example diagnostics
cargo test --locked --workspace --all-targets
node tests/bridge.mjs
```

The Node.js integration test uses a built debug bridge and a real `slangd`. It covers diagnostics, dependency changes, completion, hover, included Rust constants, and composition discovery and selection. To test a particular server executable, pass its path after `tests/bridge.mjs`. Set `SLANG_INLINE_BRIDGE` to the absolute path of a different bridge executable, such as the one installed from the VSIX.

## Editor resolution limits

The bridge handles conventional Cargo layouts and ordinary inline or file-backed Rust modules. Its source resolver supports module-level constants and same-module names or `crate::` paths. Rust configuration and macro expansion, `#[path]` modules, re-exports, and computed string expressions require Rust semantic analysis and fall outside the editor resolver's scope. Bare and raw `slang_inline!` bodies both support diagnostics, completion, and hover.
