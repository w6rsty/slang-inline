import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const workspace = fileURLToPath(new URL("../", import.meta.url));
const crate = path.join(workspace, "macro").replaceAll("\\", "/");
const exampleMain = path.join(workspace, "examples/injected_shader/src/main.rs");
const metadata = spawnSync("cargo", ["metadata", "--offline", "--locked", "--no-deps", "--format-version", "1"], {
    cwd: workspace,
    encoding: "utf8",
    timeout: 60_000,
});
assert.ifError(metadata.error);
assert.equal(metadata.status, 0, metadata.stderr);
const exampleTargets = JSON.parse(metadata.stdout).packages.flatMap((pkg) =>
    pkg.targets.filter((target) => target.name === "injected_shader")
        .map((target) => [pkg.name, target.kind, path.normalize(target.src_path)]));
assert.deepEqual(exampleTargets, [["slang-inline-injected-shader", ["bin"], exampleMain]]);

const directory = await mkdtemp(path.join(tmpdir(), "slang-inline-build-"));
const shadersPath = path.join(directory, "src/shaders.rs");
const headerPath = path.join(directory, "headers/scale.slangh");
const header = "float scale() { return 2.0; }\n";
const shaders = `use slang_inline::{SlangSource, slang_concat, slang_inline};

const BASIC: &str = r#"struct Material { float roughness; };"#;
const DETAILED: &str = r#"struct Material { float roughness; float metallic; };"#;
const BASIC_DECLS: SlangSource = slang_inline! { include { BASIC } };
const DETAILED_DECLS: SlangSource = slang_inline! { include { DETAILED } };
const SURFACE: SlangSource = slang_inline! {
    #include "../headers/scale.slangh"
    float shade(Material material) { return material.roughness * scale(); }
};
pub const SHADER: SlangSource = slang_concat!(BASIC_DECLS, SURFACE);
pub const DETAILED_SHADER: SlangSource = slang_concat!(DETAILED_DECLS, SURFACE);
`;

function cargo(command = "check", args = [], slangc = process.env.SLANGC) {
    const result = spawnSync("cargo", [command, "--offline", ...args], {
        cwd: directory,
        encoding: "utf8",
        timeout: 60_000,
        env: { ...process.env, SLANGC: slangc, CARGO_TARGET_DIR: path.join(directory, "target") },
    });
    assert.ifError(result.error);
    return { status: result.status, output: result.stdout + result.stderr };
}

try {
    await mkdir(path.dirname(shadersPath));
    await mkdir(path.dirname(headerPath));
    await writeFile(path.join(directory, "Cargo.toml"), `[package]
name = "slang-inline-build-test"
version = "0.1.0"
edition = "2024"

[features]
default = ["build-check"]
build-check = ["slang-inline/build-check"]

[build-dependencies]
slang-inline = { path = ${JSON.stringify(crate)}, default-features = false }
`);
    await writeFile(path.join(directory, "build.rs"), `#[cfg(feature = "build-check")]
#[path = "src/shaders.rs"]
mod shaders;

fn main() {
    #[cfg(feature = "build-check")]
    {
        use slang_inline::build::{Shader, check};
        use std::path::Path;
        let directory = Path::new("src");
        let shaders = [
            Shader { name: "SHADER", source: shaders::SHADER, directory },
            Shader { name: "DETAILED_SHADER", source: shaders::DETAILED_SHADER, directory },
        ];
        if let Err(error) = check(&shaders, &[]) {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
`);
    await writeFile(path.join(directory, "src/main.rs"), "fn main() {}\n");
    await writeFile(shadersPath, shaders);
    await writeFile(headerPath, header);
    const valid = cargo();
    assert.equal(valid.status, 0, valid.output);

    await writeFile(shadersPath, shaders.replace("float roughness; float metallic;", "float metallic;"));
    const brokenComposition = cargo();
    assert.notEqual(brokenComposition.status, 0);
    assert.match(brokenComposition.output, /Slang check failed for DETAILED_SHADER/);
    assert.doesNotMatch(brokenComposition.output, /Slang check failed for SHADER/);
    assert.match(brokenComposition.output, /src[/\\]shaders\.rs:9:54 \[E30027\]/);
    const brokenRun = cargo("run");
    assert.notEqual(brokenRun.status, 0);
    assert.match(brokenRun.output, /Slang check failed for DETAILED_SHADER/);

    await writeFile(shadersPath, shaders);
    const repaired = cargo();
    assert.equal(repaired.status, 0, repaired.output);

    await writeFile(headerPath, "#warning buildCheckWarning\n" + header);
    const warning = cargo();
    assert.equal(warning.status, 0, warning.output);
    assert.match(warning.output, /warning:.*buildCheckWarning/);

    // This file sits outside the shader's search directory. Cargo must have
    // received the path from Slang to notice the change.
    await writeFile(headerPath, "float scale() { return missingFactor; }\n");
    const brokenHeader = cargo();
    assert.notEqual(brokenHeader.status, 0);
    assert.match(brokenHeader.output, /scale\.slangh:1:\d+ \[E30015\][\s\S]*missingFactor/);

    const missingCompiler = path.join(directory, "unavailable-slangc");
    const missing = cargo("check", [], missingCompiler);
    assert.notEqual(missing.status, 0);
    assert.match(missing.output, /Could not run[\s\S]*Set SLANGC/);
    const unchecked = cargo("check", ["--no-default-features"], missingCompiler);
    assert.equal(unchecked.status, 0, unchecked.output);
    console.log("Default Cargo Slang checks, composition errors, dependency changes and opt-out passed.");
} finally {
    await rm(directory, { recursive: true, force: true });
}
