const vscode = require("vscode");

function toRange({ start, end }) {
    return new vscode.Range(start.line, start.character, end.line, end.character);
}

function markup(content) {
    if (typeof content === "string") return new vscode.MarkdownString(content);
    if (typeof content?.value !== "string") return undefined;
    const text = new vscode.MarkdownString();
    if (content.kind === "plaintext") return text.appendText(content.value);
    if (typeof content.language === "string") return text.appendCodeblock(content.value, content.language);
    return text.appendMarkdown(content.value);
}

function toCompletionList(result) {
    const entries = Array.isArray(result) ? result : result?.items;
    if (!entries) return undefined;
    const items = entries.map((entry) => {
        const kind = entry.kind >= 1 && entry.kind <= 25 ? entry.kind - 1 : undefined;
        const item = new vscode.CompletionItem(entry.label, kind);
        const edit = entry.textEdit;
        item.insertText = edit.newText;
        item.range = edit.range
            ? toRange(edit.range)
            : { inserting: toRange(edit.insert), replacing: toRange(edit.replace) };
        item.detail = entry.detail;
        item.documentation = markup(entry.documentation);
        item.filterText = entry.filterText;
        item.sortText = entry.sortText;
        item.preselect = entry.preselect;
        item.commitCharacters = entry.commitCharacters;
        item.tags = entry.tags;
        item.keepWhitespace = entry.insertTextMode === 1;
        item.additionalTextEdits = entry.additionalTextEdits?.map(({ range, newText }) =>
            vscode.TextEdit.replace(toRange(range), newText));
        return item;
    });
    return new vscode.CompletionList(items, result.isIncomplete ?? false);
}

function toHover(result) {
    if (!result) return undefined;
    const contents = (Array.isArray(result.contents) ? result.contents : [result.contents])
        .map(markup)
        .filter(Boolean);
    return contents.length
        ? new vscode.Hover(contents, result.range ? toRange(result.range) : undefined)
        : undefined;
}

module.exports = { toRange, toCompletionList, toHover };
