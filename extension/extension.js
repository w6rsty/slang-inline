const { spawn } = require("node:child_process");
const { existsSync } = require("node:fs");
const path = require("node:path");
const readline = require("node:readline");
const vscode = require("vscode");
const { toRange, toCompletionList, toHover } = require("./language-features");

const dependencyExtensions = ["slang", "slangh", "hlsl", "hlsli", "h", "inc"];
const dependencyGlob = `**/*.{${dependencyExtensions.join(",")}}`;

function isDependency(document) {
    const extension = path.extname(document.uri.fsPath).slice(1).toLowerCase();
    return document.uri.scheme === "file" && dependencyExtensions.includes(extension);
}

function hasSlang(document) {
    const text = document.getText();
    return text.includes("slang_inline") || text.includes("slang_concat");
}

function activate(context) {
    const diagnostics = vscode.languages.createDiagnosticCollection("slang-inline");
    const log = vscode.window.createOutputChannel("Slang Inline Diagnostics");
    const pending = new Map();
    const revisions = new Map();
    const queries = new Map();
    const diagnosticsByOwner = new Map();
    const changedRustFiles = new Set();
    let nextRevision = 0;
    let nextQuery = 1;
    let bridge;
    let dependencyTimer;
    let rustFilesTimer;
    let dependencyWatchers = [];

    function isRustFile(document) {
        return document.languageId === "rust" && document.uri.scheme === "file";
    }

    function send(child, message) {
        if (child.stdin.writable) {
            child.stdin.write(JSON.stringify(message) + "\n");
        }
    }

    function syncDependencies(child) {
        const documents = vscode.workspace.textDocuments
            .filter((document) => document.isDirty && isDependency(document))
            .map((document) => ({ uri: document.uri.toString(), text: document.getText() }));
        send(child, { type: "refreshDependencies", documents });
    }

    function scheduleDependencies() {
        if (!bridge) return;
        clearTimeout(dependencyTimer);
        dependencyTimer = setTimeout(() => {
            dependencyTimer = undefined;
            if (bridge) syncDependencies(bridge);
        }, 150);
    }

    function stopWatchingDependencies() {
        clearTimeout(dependencyTimer);
        dependencyTimer = undefined;
        clearTimeout(rustFilesTimer);
        rustFilesTimer = undefined;
        changedRustFiles.clear();
        dependencyWatchers.forEach((watcher) => watcher.dispose());
        dependencyWatchers = [];
    }

    function flushRustFiles(child) {
        clearTimeout(rustFilesTimer);
        rustFilesTimer = undefined;
        if (!changedRustFiles.size) return;
        send(child, { type: "rustFilesChanged", uris: [...changedRustFiles] });
        changedRustFiles.clear();
    }

    function scheduleRustFile(uri) {
        if (!bridge) return;
        changedRustFiles.add(uri.toString());
        clearTimeout(rustFilesTimer);
        rustFilesTimer = setTimeout(() => flushRustFiles(bridge), 150);
    }

    function startBridge() {
        const config = vscode.workspace.getConfiguration("slangdInline");
        const program = process.platform === "win32"
            ? "slang-inline-bridge.exe"
            : "slang-inline-bridge";
        const bundled = path.join(context.extensionPath, "bin", program);
        const executable = config.get("bridgePath")
            || (existsSync(bundled) ? bundled : path.resolve(context.extensionPath, "..", "target", "debug", program));
        const slangd = config.get("slangdPath", "slangd") || "slangd";
        const folders = (vscode.workspace.workspaceFolders || [])
            .filter((folder) => folder.uri.scheme === "file");
        const roots = folders.map((folder) => folder.uri.fsPath);
        const searchPaths = [...new Set(config.get("searchPaths", []).flatMap((directory) => {
            if (!directory.trim()) return [];
            if (path.isAbsolute(directory)) return [path.normalize(directory)];
            return roots.map((root) => path.resolve(root, directory));
        }))];
        const child = spawn(executable, [slangd], {
            cwd: folders[0]?.uri.fsPath || context.extensionPath,
            windowsHide: true,
        });
        bridge = child;
        child.stderr.setEncoding("utf8");
        child.stderr.on("data", (message) => log.append(message));
        const lines = readline.createInterface({ input: child.stdout });
        lines.on("line", (line) => {
            if (bridge !== child) return;
            try {
                const message = JSON.parse(line);
                if (message.type === "queryResult") {
                    queries.get(message.id)?.(message.result);
                } else {
                    showDiagnostics(message);
                }
            } catch (error) {
                log.appendLine(String(error));
            }
        });
        function failed(message) {
            if (bridge !== child) return;
            log.appendLine(message);
            bridge = undefined;
            child.stdin.end();
            stopWatchingDependencies();
            queries.forEach((finish) => finish());
            revisions.clear();
            diagnosticsByOwner.clear();
            diagnostics.clear();
        }
        child.on("error", (error) => failed(String(error)));
        child.stdin.on("error", (error) => failed(String(error)));
        child.on("exit", (code) => failed(`Slang bridge exited (${code}).`));
        send(child, {
            type: "initialize",
            folders: folders.map((folder) => ({ uri: folder.uri.toString(), name: folder.name })),
            searchPaths,
        });
        syncDependencies(child);

        for (const root of new Set([...roots, ...searchPaths])) {
            const watcher = vscode.workspace.createFileSystemWatcher(new vscode.RelativePattern(root, dependencyGlob));
            watcher.onDidCreate(scheduleDependencies);
            watcher.onDidChange(scheduleDependencies);
            watcher.onDidDelete(scheduleDependencies);
            dependencyWatchers.push(watcher);
        }
        for (const root of roots) {
            const watcher = vscode.workspace.createFileSystemWatcher(new vscode.RelativePattern(root, "**/*.rs"));
            watcher.onDidCreate(scheduleRustFile);
            watcher.onDidChange(scheduleRustFile);
            watcher.onDidDelete(scheduleRustFile);
            dependencyWatchers.push(watcher);
        }
    }

    function showDiagnostics(message) {
        if (message.type !== "diagnostics" || revisions.get(message.uri)?.revision !== message.version) return;
        const byFile = new Map();
        for (const item of message.diagnostics) {
            const uri = item.uri ? vscode.Uri.parse(item.uri).toString() : message.uri;
            if (!byFile.has(uri)) byFile.set(uri, []);
            byFile.get(uri).push(item);
        }
        replaceDiagnostics(message.uri, byFile);
    }

    function replaceDiagnostics(owner, byFile = new Map()) {
        const affected = new Set([...(diagnosticsByOwner.get(owner)?.keys() || []), ...byFile.keys()]);
        if (byFile.size) {
            diagnosticsByOwner.set(owner, byFile);
        } else {
            diagnosticsByOwner.delete(owner);
        }
        for (const uri of affected) {
            const seen = new Set();
            const items = [];
            for (const files of diagnosticsByOwner.values()) {
                for (const { range, message, severity, code } of files.get(uri) || []) {
                    const key = JSON.stringify([range, message, severity, code]);
                    if (seen.has(key)) continue;
                    seen.add(key);
                    const item = new vscode.Diagnostic(
                        toRange(range), message,
                        severity == null ? vscode.DiagnosticSeverity.Error : severity - 1,
                    );
                    item.source = "slang-inline";
                    item.code = code ?? undefined;
                    items.push(item);
                }
            }
            diagnostics.set(vscode.Uri.parse(uri), items);
        }
    }

    function synchronize(document) {
        const uri = document.uri.toString();
        clearTimeout(pending.get(uri));
        pending.delete(uri);
        const revision = ++nextRevision;
        revisions.set(uri, { revision, documentVersion: document.version });
        send(bridge, {
            type: "update",
            uri,
            directoryUri: vscode.Uri.file(path.dirname(document.uri.fsPath)).toString(),
            version: revision,
            text: document.getText(),
        });
        return revision;
    }

    function startAndSynchronize() {
        startBridge();
        vscode.workspace.textDocuments.filter(isRustFile).forEach(synchronize);
    }

    function request(document, token, payload) {
        if (!isRustFile(document) || token.isCancellationRequested) return undefined;
        if (!bridge) {
            if (!hasSlang(document)) return undefined;
            startAndSynchronize();
        }
        const child = bridge;
        if (dependencyTimer) {
            clearTimeout(dependencyTimer);
            dependencyTimer = undefined;
            syncDependencies(child);
        }
        if (rustFilesTimer) flushRustFiles(child);
        const uri = document.uri.toString();
        for (const other of vscode.workspace.textDocuments) {
            if (other !== document && pending.has(other.uri.toString()) && isRustFile(other)) synchronize(other);
        }
        const documentVersion = document.version;
        const snapshot = revisions.get(uri);
        const revision = snapshot && snapshot.documentVersion === documentVersion
            ? snapshot.revision
            : synchronize(document);
        const id = ++nextQuery;
        return new Promise((resolve) => {
            let cancellation;
            const finish = (result) => {
                queries.delete(id);
                clearTimeout(timeout);
                cancellation?.dispose();
                const current = bridge === child && document.version === documentVersion
                    && revisions.get(uri)?.revision === revision && !token.isCancellationRequested;
                resolve(current ? result : undefined);
            };
            const cancel = () => {
                if (!queries.has(id)) return;
                if (bridge === child) send(child, { type: "cancelQuery", id });
                finish();
            };
            const timeout = setTimeout(cancel, 5000);
            queries.set(id, finish);
            cancellation = token.onCancellationRequested(cancel);
            if (!queries.has(id)) {
                cancellation.dispose();
                return;
            }
            if (token.isCancellationRequested) {
                cancel();
                return;
            }
            send(child, {
                ...payload,
                id,
                uri,
                version: revision,
            });
        });
    }

    function query(document, position, token, feature, context) {
        return request(document, token, {
            type: "query",
            feature,
            position: { line: position.line, character: position.character },
            context,
        });
    }

    async function selectComposition() {
        const document = vscode.window.activeTextEditor?.document;
        if (!document || !isRustFile(document)) {
            vscode.window.showInformationMessage("Open a Rust file containing a Slang fragment.");
            return;
        }
        const cancellation = new vscode.CancellationTokenSource();
        let result;
        try {
            result = await request(document, cancellation.token, { type: "listContexts" });
        } finally {
            cancellation.dispose();
        }
        if (!result?.items.length) {
            vscode.window.showInformationMessage("No Slang compositions were found for this Rust file.");
            return;
        }
        const child = bridge;
        const uri = document.uri.toString();
        const version = revisions.get(uri)?.revision;
        const choices = result.items.map((item) => {
            const location = `${vscode.workspace.asRelativePath(vscode.Uri.parse(item.uri), true)}:${item.line + 1}`;
            return {
                id: item.id,
                label: item.name || location,
                description: item.id === result.selected ? "Current" : undefined,
                detail: item.name ? location : undefined,
            };
        });
        const choice = await vscode.window.showQuickPick(choices, { placeHolder: "Select a Slang composition" });
        if (choice && child && bridge === child && revisions.get(uri)?.revision === version) {
            send(child, { type: "selectContext", uri, version, context: choice.id });
        }
    }

    function schedule(document) {
        if (!isRustFile(document)) return;
        const uri = document.uri.toString();
        revisions.delete(uri);
        replaceDiagnostics(uri);
        clearTimeout(pending.get(uri));
        pending.set(uri, setTimeout(() => {
            if (bridge) {
                synchronize(document);
            } else if (hasSlang(document)) {
                startAndSynchronize();
            } else {
                pending.delete(uri);
            }
        }, 150));
    }

    function close(document) {
        const uri = document.uri.toString();
        if (!revisions.has(uri) && !pending.has(uri)) return;
        clearTimeout(pending.get(uri));
        pending.delete(uri);
        revisions.delete(uri);
        replaceDiagnostics(uri);
        if (bridge) send(bridge, { type: "close", uri });
    }

    function stopBridge() {
        const child = bridge;
        bridge = undefined;
        child?.stdin.end();
        queries.forEach((finish) => finish());
        stopWatchingDependencies();
        pending.forEach(clearTimeout);
        pending.clear();
        revisions.clear();
        diagnosticsByOwner.clear();
        diagnostics.clear();
    }

    function restartBridge() {
        stopBridge();
        vscode.workspace.textDocuments.forEach(schedule);
    }

    context.subscriptions.push(
        { dispose: stopBridge },
        diagnostics,
        log,
        vscode.commands.registerCommand("slangInline.selectComposition", selectComposition),
        vscode.languages.registerCompletionItemProvider({ language: "rust", scheme: "file" }, {
            async provideCompletionItems(document, position, token, context) {
                return toCompletionList(await query(document, position, token, "completion", {
                    triggerKind: context.triggerKind + 1,
                    triggerCharacter: context.triggerCharacter,
                }));
            },
        }, "."),
        vscode.languages.registerHoverProvider({ language: "rust", scheme: "file" }, {
            async provideHover(document, position, token) {
                return toHover(await query(document, position, token, "hover"));
            },
        }),
        vscode.workspace.onDidOpenTextDocument((document) => {
            schedule(document);
            if (document.isDirty && isDependency(document)) scheduleDependencies();
        }),
        vscode.workspace.onDidChangeTextDocument(({ document, contentChanges }) => {
            if (!contentChanges.length) return;
            schedule(document);
            if (isDependency(document)) scheduleDependencies();
        }),
        vscode.workspace.onDidSaveTextDocument((document) => {
            if (isDependency(document)) scheduleDependencies();
        }),
        vscode.workspace.onDidCloseTextDocument((document) => {
            close(document);
            if (isDependency(document)) scheduleDependencies();
        }),
        vscode.workspace.onDidChangeWorkspaceFolders(restartBridge),
        vscode.workspace.onDidChangeConfiguration((event) => {
            if (event.affectsConfiguration("slangdInline")) restartBridge();
        }),
    );
    vscode.workspace.textDocuments.forEach(schedule);
}

module.exports = { activate };
