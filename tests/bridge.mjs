import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdir, mkdtemp, readdir, rmdir, unlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import readline from "node:readline";
import { fileURLToPath, pathToFileURL } from "node:url";

const binary = process.platform === "win32" ? "slang-inline-bridge.exe" : "slang-inline-bridge";
const executable = process.env.SLANG_INLINE_BRIDGE
    || fileURLToPath(new URL(`../target/debug/${binary}`, import.meta.url));
const directory = await mkdtemp(path.join(tmpdir(), "slang-inline-test-"));
const workspace = path.join(directory, "project");
const shaders = path.join(directory, "shader libs");
await mkdir(workspace);
await mkdir(shaders);
const directoryUri = pathToFileURL(workspace).href;
const uri = pathToFileURL(path.join(workspace, "source.rs")).href;
const rustDirectory = path.join(workspace, "src");
const rustDirectoryUri = pathToFileURL(rustDirectory).href;
const rustHost = pathToFileURL(path.join(rustDirectory, "lib.rs")).href;
const manifest = path.join(workspace, "Cargo.toml");
const material = path.join(rustDirectory, "material.rs");
const materialUri = pathToFileURL(material).href;
const lightingDirectory = path.join(rustDirectory, "lighting");
const lightingModule = path.join(lightingDirectory, "mod.rs");
const lighting = path.join(lightingDirectory, "values.rs");
const library = path.join(rustDirectory, "library.rs");
const surface = path.join(rustDirectory, "surface.rs");
const surfaceUri = pathToFileURL(surface).href;
const root = fileURLToPath(rustHost);
const compositions = path.join(rustDirectory, "compositions.rs");
const compositionsUri = pathToFileURL(compositions).href;
const materialSource = 'pub const MATERIAL_DECLS: &str = r#"\nstruct Material {\n    float roughness;\n};\n"#;\n';
await mkdir(rustDirectory);
await mkdir(lightingDirectory);
await writeFile(manifest, '[package]\nname = "inline-fixture"\nversion = "0.1.0"\n');
await writeFile(material, materialSource);
await writeFile(lightingModule, "pub mod values;\n");
await writeFile(lighting, 'pub const LIGHT_DECLS: &str = r#"struct Light { Material material; };"#;\n');
await writeFile(library, [
    "pub const LIBRARY: SlangSource = slang_inline! {",
    "    include { crate::material::MATERIAL_DECLS, crate::lighting::values::LIGHT_DECLS, }",
    "};",
].join("\n"));
const combinedLine = "float shade(Light light) { return light.material.roughness + combinedSentinel; }";
const surfaceText = [
    "pub const SURFACE: SlangSource = slang_inline! {",
    combinedLine,
    "};",
].join("\n");
const surfaceLine = surfaceText.split("\n").indexOf(combinedLine);
await writeFile(surface, surfaceText);
const shared = path.join(shaders, "shared.slang");
const header = path.join(shaders, "inline.slangh");
const originalModule = "public float probe(float x) { return x; }\n";
const changedModule = "public float probe(float x, float y) { return x + y; }\n";
const originalHeader = "float scale() { return 2; }\n";
await writeFile(shared, originalModule);
await writeFile(header, originalHeader);
const child = spawn(executable, [process.argv[2] || "slangd"], {
    cwd: workspace,
    stdio: ["pipe", "pipe", "inherit"],
    windowsHide: true,
});
const exited = once(child, "exit");
const lines = readline.createInterface({ input: child.stdout });
const messages = lines[Symbol.asyncIterator]();
const deadline = setTimeout(() => child.kill(), 45_000);

function send(message) {
    child.stdin.write(JSON.stringify(message) + "\n");
}

function update(version, text, target = uri, directory = directoryUri) {
    send({ type: "update", uri: target, directoryUri: directory, text, version });
}

const lastDiagnostics = new Map();
function readEvent(value) {
    const message = JSON.parse(value);
    if (message.type === "diagnostics") lastDiagnostics.set(message.uri, message.diagnostics);
    return message;
}

async function expect(version, predicate, owner = uri) {
    for (;;) {
        const { value, done } = await messages.next();
        assert.ok(!done, `bridge exited while waiting for diagnostics for revision ${version}: ${JSON.stringify(lastDiagnostics.get(owner))}`);
        const message = readEvent(value);
        if (message.type === "diagnostics" && message.uri === owner
            && message.version === version && predicate(message.diagnostics)) {
            return message.diagnostics;
        }
    }
}

let nextQuery = 1;
async function request(version, owner, payload) {
    const id = ++nextQuery;
    send({ ...payload, id, uri: owner, version });
    for (;;) {
        const { value, done } = await messages.next();
        assert.ok(!done, `bridge exited while waiting for ${payload.feature ?? payload.type}`);
        const message = readEvent(value);
        if (message.type === "queryResult" && message.id === id) {
            return message.result;
        }
    }
}

function query(version, feature, position, context, owner = uri) {
    return request(version, owner, { type: "query", feature, position, context });
}

try {
    send({
        type: "initialize",
        folders: [{ uri: directoryUri, name: "test" }],
        searchPaths: [shaders],
    });
    const first = "/* 🦀 */ slang_inline! { float first() { return 1.0 } }";
    const second = 'slang_inline! { r#"float second() { return 2.0 }"# }';
    update(1, [first, second].join("\r\n"));
    const found = await expect(1, (items) => items.length === 2);
    for (const [line, text] of [first, second].entries()) {
        assert.match(found[line].message, /expected ';'/);
        assert.deepEqual(found[line].range.start, { line, character: text.indexOf("}") });
    }

    update(2, ["// move both blocks", first, second].join("\n"));
    const moved = await expect(2, (items) => items.length === 2);
    assert.deepEqual(moved.map((item) => item.range.start.line), [1, 2]);

    const fixed = first.replace("1.0", "1.0;");
    update(3, [fixed, second].join("\n"));
    const remaining = await expect(3, (items) => items.length === 1);
    assert.equal(remaining[0].range.start.line, 1);

    update(4, fixed);
    assert.deepEqual(await expect(4, () => true), []);
    update(5, first);
    const restored = await expect(5, (items) => items.length === 1);
    assert.equal(restored[0].range.start.line, 0);
    update(6, "fn main() {}");
    assert.deepEqual(await expect(6, () => true), []);

    const call = "    float run() { return probe(scale()) + testSentinel; }";
    update(7, ["slang_inline! {", "    import shared;", '    #include "inline.slangh"', call, "}"].join("\n"));
    const hasSentinel = (items) => items.some((item) => item.message.includes("testSentinel"));
    const onlySentinel = (items) => items.length === 1 && hasSentinel(items);
    assert.equal((await expect(7, hasSentinel)).length, 1);

    await writeFile(shared, changedModule);
    send({ type: "refreshDependencies", documents: [] });
    const changed = await expect(7, hasSentinel);
    const callError = changed.find((item) => item.message.includes("got 1, expected 2"));
    assert.ok(callError);
    assert.equal(callError.range.start.line, 3);
    assert.ok(callError.range.start.character >= call.indexOf("probe"));

    send({ type: "refreshDependencies", documents: [{
        uri: pathToFileURL(shared).href,
        text: [
            "public struct Input { float value; };",
            "public float probe(Input input) { return input.value; }",
        ].join("\n"),
    }] });
    const wrongType = (await expect(7, hasSentinel))
        .find((item) => item.message.includes("Input") && item.message.includes("float"));
    assert.ok(wrongType);
    assert.equal(wrongType.range.start.line, 3);
    assert.ok(wrongType.range.start.character >= call.indexOf("probe"));

    const moduleOverlay = { uri: pathToFileURL(shared).href, text: originalModule };
    const headerOverlay = {
        uri: pathToFileURL(header).href,
        text: "float scale(float x) { return x; }\n",
    };
    send({ type: "refreshDependencies", documents: [moduleOverlay, headerOverlay] });
    const unsaved = await expect(7, hasSentinel);
    assert.equal(unsaved.length, 2);
    assert.ok(unsaved.some((item) => item.message.includes("got 0, expected 1")));
    send({ type: "refreshDependencies", documents: [moduleOverlay] });
    await expect(7, onlySentinel);

    send({ type: "refreshDependencies", documents: [] });
    assert.ok((await expect(7, hasSentinel)).some((item) => item.message.includes("got 1, expected 2")));
    await writeFile(shared, originalModule);
    send({ type: "refreshDependencies", documents: [] });
    await expect(7, onlySentinel);

    // Some versions publish both undefined identifiers under shared.slang.
    // A later edit still needs to receive the diagnostic at the new inline URI.
    await writeFile(shared, originalModule + "struct Imported { MissingType<float4> value; };\n");
    send({ type: "refreshDependencies", documents: [] });
    await expect(7, hasSentinel);
    update(8, ["slang_inline! {", "    import shared;", '    #include "inline.slangh"',
        call.replace("testSentinel", "secondSentinel"), "}"].join("\n"));
    const next = await expect(8, (items) => items.some((item) => item.message.includes("secondSentinel")));
    assert.equal(next.find((item) => item.message.includes("secondSentinel")).range.start.line, 3);

    await writeFile(shared, originalModule);
    send({ type: "refreshDependencies", documents: [] });
    const bare = "/* 🦀 */ slang_inline! { Texture2D<float4> icon; float4 sample() { return icon.Load; } }";
    const rawLine = "/* 🦀 */ float4 sample() { return overlay.Lo; }";
    update(9, [
        "fn rust_only() {}",
        bare,
        'slang_inline! { r##"Texture2D<float4> overlay;',
        rawLine,
        '"## }',
    ].join("\r\n"));
    for (const [line, source, prefix] of [[1, bare, "Load"], [3, rawLine, "Lo"]]) {
        const start = source.lastIndexOf(prefix);
        const character = start + Math.min(prefix.length, 3);
        const completions = await query(9, "completion", { line, character }, { triggerKind: 1 });
        const choices = completions?.items ?? completions;
        const load = choices?.find((item) => item.label === "Load");
        assert.ok(load);
        const edit = load.textEdit;
        assert.deepEqual(edit.range, {
            start: { line, character: start },
            end: { line, character: start + prefix.length },
        });
        assert.equal(source.slice(0, edit.range.start.character) + edit.newText
            + source.slice(edit.range.end.character), source.replace(prefix, "Load"));
    }
    const dot = await query(9, "completion", { line: 1, character: bare.lastIndexOf(".") + 1 },
        { triggerKind: 2, triggerCharacter: "." });
    assert.ok((dot?.items ?? dot)?.some((item) => item.label === "Load"));

    const identifier = rawLine.indexOf("overlay");
    const hover = await query(9, "hover", { line: 3, character: identifier + 2 });
    assert.match(JSON.stringify(hover.contents), /Texture2D/);
    assert.deepEqual(hover.range, {
        start: { line: 3, character: identifier },
        end: { line: 3, character: identifier + "overlay".length },
    });
    assert.equal(await query(9, "hover", { line: 0, character: 8 }), null);
    assert.equal(await query(9, "completion", { line: 4, character: 3 }, { triggerKind: 1 }), null);
    assert.equal(await query(8, "hover", { line: 3, character: identifier + 2 }), null);

    send({ type: "close", uri });
    const injectedLine = "float shade(Light light) { return light.material.roughness * INJECTED_SCALE + injectedSentinel; }";
    const sourceLines = [
        "mod material;",
        "mod lighting;",
        "fn shader() -> SlangSource {",
        "    slang_inline! {",
        "        include { crate::material::MATERIAL_DECLS, crate::lighting::values::LIGHT_DECLS, }",
        '        #include "inline.slangh"',
        "        #define INJECTED_SCALE scale()",
        injectedLine,
        "    }",
        "}",
    ];
    const shaderLine = sourceLines.indexOf(injectedLine);
    const fieldColumn = injectedLine.indexOf("roughness");
    const completeField = async () => {
        const result = await query(1, "completion", { line: shaderLine, character: fieldColumn },
            { triggerKind: 2, triggerCharacter: "." }, rustHost);
        return result?.items ?? result;
    };
    const hasInjectedSentinel = (items) => items.some((item) => item.message.includes("injectedSentinel"));
    const onlyInjectedSentinel = (items) => items.length === 1 && hasInjectedSentinel(items);
    update(1, sourceLines.join("\r\n"), rustHost, rustDirectoryUri);
    await expect(1, onlyInjectedSentinel, rustHost);
    const originalField = (await completeField()).find((item) => item.label === "roughness");
    assert.deepEqual(originalField.textEdit.range.start, { line: shaderLine, character: fieldColumn });

    update(1, materialSource.replace("roughness", "finish"), materialUri, rustDirectoryUri);
    const changedField = await expect(1, (items) => items.some((item) => item.message.includes("roughness")), rustHost);
    const oldField = changedField.find((item) => item.message.includes("roughness"));
    assert.equal(oldField.uri, undefined);
    assert.equal(oldField.range.start.line, shaderLine);
    const updatedFields = await completeField();
    assert.ok(updatedFields.some((item) => item.label === "finish"));
    assert.ok(!updatedFields.some((item) => item.label === "roughness"));

    update(2, materialSource.replace("    float roughness;", "    float roughness"), materialUri, rustDirectoryUri);
    const isInMaterial = (item) => item.uri && fileURLToPath(item.uri) === material;
    const declarationError = (await expect(1, (items) => items.some(isInMaterial), rustHost)).find(isInMaterial);
    assert.match(declarationError.message, /expected ';'/);
    assert.equal(declarationError.range.start.line, 3);

    send({ type: "close", uri: materialUri });
    await expect(1, onlyInjectedSentinel, rustHost);
    await writeFile(material, materialSource.replace("roughness", "gloss"));
    send({ type: "rustFilesChanged", uris: [materialUri] });
    await expect(1, (items) => items.some((item) => item.message.includes("roughness")), rustHost);
    assert.ok((await completeField()).some((item) => item.label === "gloss"));

    await writeFile(material, materialSource);
    update(2, [
        "mod material;",
        "mod lighting;",
        "mod library;",
        "mod surface;",
        "const PARTIAL: SlangSource = slang_concat!(crate::library::LIBRARY);",
        "const SHADER: SlangSource = slang_concat!(PARTIAL, crate::surface::SURFACE);",
    ].join("\n"), rustHost, rustDirectoryUri);
    update(1, surfaceText, surfaceUri, rustDirectoryUri);
    const composed = await expect(2, (items) => items.some((item) => item.message.includes("combinedSentinel")), rustHost);
    assert.equal(composed.length, 1);
    assert.equal(fileURLToPath(composed[0].uri), surface);
    assert.equal(composed[0].range.start.line, surfaceLine);
    const completeComposed = async () => {
        const result = await query(1, "completion", { line: surfaceLine, character: combinedLine.indexOf("roughness") },
            { triggerKind: 2, triggerCharacter: "." }, surfaceUri);
        return result?.items ?? result;
    };
    const member = (await completeComposed()).find((item) => item.label === "roughness");
    assert.deepEqual(member.textEdit.range.start, { line: surfaceLine, character: combinedLine.indexOf("roughness") });
    assert.deepEqual(lastDiagnostics.get(surfaceUri), []);

    update(1, materialSource.replace("roughness", "gloss"), materialUri, rustDirectoryUri);
    const revised = await expect(2, (items) => items.some((item) => item.message.includes("roughness")), rustHost);
    const oldMember = revised.find((item) => item.message.includes("roughness"));
    assert.equal(fileURLToPath(oldMember.uri), surface);
    assert.ok((await completeComposed()).some((item) => item.label === "gloss"));
    send({ type: "close", uri: materialUri });
    send({ type: "close", uri: rustHost });
    await expect(1, (items) => items.some((item) => item.message.includes("Light")), surfaceUri);

    // Only surface.rs is open. The other fragments and both entry points are on disk.
    const closedRoot = "mod material; mod lighting; mod library; mod surface; mod compositions;\n";
    const firstComposition = "const FIRST: SlangSource = slang_concat!(crate::library::LIBRARY, crate::surface::SURFACE);";
    const alternatives = [
        "const OTHER: SlangSource = slang_inline! { struct Material { float gloss; }; struct Light { Material material; }; };",
        firstComposition,
        "const SECOND: SlangSource = slang_concat!(OTHER, crate::surface::SURFACE);",
    ].join("\n");
    await writeFile(root, closedRoot);
    await writeFile(compositions, alternatives);
    send({ type: "rustFilesChanged", uris: [rustHost, compositionsUri] });
    const onlyCombinedSentinel = (items) => items.length === 1 && items[0].message.includes("combinedSentinel");
    const closedDiagnostics = await expect(1, onlyCombinedSentinel, surfaceUri);
    assert.equal(closedDiagnostics[0].uri, undefined);
    assert.equal(closedDiagnostics[0].range.start.line, surfaceLine);
    assert.ok((await completeComposed()).some((item) => item.label === "roughness"));
    const contexts = await request(1, surfaceUri, { type: "listContexts" });
    assert.deepEqual(contexts.items.map(({ uri, line, name }) => [fileURLToPath(uri), line, name]),
        [[compositions, 1, "FIRST"], [compositions, 2, "SECOND"]]);
    assert.equal(contexts.selected, contexts.items[0].id);

    const secondContext = contexts.items[1].id;
    send({ type: "selectContext", uri: surfaceUri, version: 1, context: secondContext });
    const onlyWrongField = (items) => items.some((item) => item.message.includes("roughness"))
        && !items.some((item) => item.message.includes("undefined identifier 'Light'"));
    await expect(1, onlyWrongField, surfaceUri);
    assert.ok((await completeComposed()).some((item) => item.label === "gloss"));
    assert.equal((await request(1, surfaceUri, { type: "listContexts" })).selected, secondContext);

    update(1, "\n" + alternatives, compositionsUri, rustDirectoryUri);
    const openDiagnostics = await expect(1, onlyWrongField, compositionsUri);
    assert.ok(openDiagnostics.every((item) => fileURLToPath(item.uri) === surface));
    assert.ok((await completeComposed()).some((item) => item.label === "gloss"));
    send({ type: "close", uri: compositionsUri });
    await expect(1, onlyWrongField, surfaceUri);

    await writeFile(compositions, firstComposition);
    send({ type: "rustFilesChanged", uris: [compositionsUri] });
    await expect(1, onlyCombinedSentinel, surfaceUri);
    assert.equal((await request(1, surfaceUri, { type: "listContexts" })).items.length, 1);
    assert.ok((await completeComposed()).some((item) => item.label === "roughness"));

    send({ type: "close", uri: surfaceUri });
    child.stdin.end();
    assert.deepEqual(await exited, [0, null]);
    assert.deepEqual((await readdir(workspace)).sort(), ["Cargo.toml", "src"]);
    assert.deepEqual((await readdir(rustDirectory)).sort(), ["compositions.rs", "lib.rs", "library.rs", "lighting", "material.rs", "surface.rs"]);
    assert.deepEqual((await readdir(shaders)).sort(), ["inline.slangh", "shared.slang"]);
    console.log("Live slangd diagnostics, completion, hover, injection and composition passed.");
} finally {
    child.stdin.end();
    await exited;
    clearTimeout(deadline);
    await unlink(header);
    await unlink(shared);
    await unlink(manifest);
    await unlink(material);
    for (const file of [root, compositions]) {
        await unlink(file).catch((error) => { if (error.code !== "ENOENT") throw error; });
    }
    await unlink(library);
    await unlink(surface);
    await unlink(lighting);
    await unlink(lightingModule);
    await rmdir(lightingDirectory);
    await rmdir(rustDirectory);
    await rmdir(shaders);
    await rmdir(workspace);
    await rmdir(directory);
}
