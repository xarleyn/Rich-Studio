// Only the languages and editor features used by Rich Studio. Vendored, not a build step for the app.
export { EditorState, EditorSelection, Transaction, StateEffect } from '@codemirror/state';
export { EditorView, keymap, lineNumbers, highlightActiveLineGutter, drawSelection, dropCursor, highlightActiveLine } from '@codemirror/view';
export { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
export { syntaxTree, syntaxHighlighting, HighlightStyle, bracketMatching, indentOnInput, foldGutter, foldKeymap, indentUnit } from '@codemirror/language';
export { tags } from '@lezer/highlight';
export { autocompletion, completionKeymap, closeBrackets, closeBracketsKeymap, startCompletion, acceptCompletion, CompletionContext } from '@codemirror/autocomplete';
export { searchKeymap, highlightSelectionMatches, openSearchPanel } from '@codemirror/search';
export { linter, lintGutter } from '@codemirror/lint';
export { html, htmlCompletionSourceWith } from '@codemirror/lang-html';
export { markdown, markdownLanguage } from '@codemirror/lang-markdown';
export { json, jsonParseLinter } from '@codemirror/lang-json';
