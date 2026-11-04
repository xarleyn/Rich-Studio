# Templates in the Rust backend

The Rust template slice uses the unchanged `post_templates` table from migration 0015. HTTP and MCP call the same `TemplateService` with the request's explicit `AccountContext`. SQLite work runs in an awaited blocking worker. Creating a draft never publishes, copies a channel selection, or retains `_studio` editor buffers.

The six HTTP operations preserve `/api/templates`, `/api/templates/{tid}` and `/api/templates/{tid}/drafts`, including 201 for creation, 409 for a stale version, 404 for missing or other-account records, 422 for invalid fields and 413 for the template, expansion or database limits. The four MCP tools are `list_post_templates`, `get_post_template`, `save_post_template` and `draft_from_template`; the last returns the existing editor and preview links.

Substitution operates once on string values. Blocks keep literal text; HTML values are escaped; Markdown punctuation becomes numeric character references. Variables in JSON keys, HTML names, unquoted attributes, event handlers, untyped URL destinations, Markdown code or Markdown math are rejected. Literal upload references and complete field defaults remain visible to existing media retention logic. Fragmented defaults cannot synthesize an otherwise invisible upload reference.

The 1 MiB expanded JSON limit is measured before the result is constructed. JSON size accounting retains Python's spaces after commas and colons. Updates and deletes use the read version in the SQL write condition. Creating a draft requires the form's version and returns a separate draft each time.

`cargo test --locked --test templates` exercises preservation, literal substitution, malformed/unsafe inputs, code and math boundaries, required/default/URL fields, bounded expansion, the 200-template limit, concurrent writers, old rows, account isolation, media retention, HTTP statuses and MCP links. It uses temporary data directories and makes no Telegram requests. Authentication and role checks are supplied by the shared HTTP/MCP guard and its access tests.
