// Shapes of what the studio API returns, for tsc --checkJs. Types only: nothing here ships to the browser.
// Post content (blocks, entities, media items) stays loosely typed on purpose: its fields come from the
// Bot API spec at runtime, and the editor must accept fields this file has never heard of.

type Json = Record<string, any>;

/** GET /api/imports/telegram */
interface TelegramImportStatus {
  mtproto_configured: boolean;
  own_session: boolean;
  bot_username: string;
  forward_available: boolean;
}

/** POST /api/imports/telegram; also the result of a completed forward import. */
interface TelegramImportResult {
  draft: DraftRow & { payload: Json };
  source: { url: string; username: string | null; channel_id: number | null; message_id: number; topic_id: number | null; single: boolean; provider: 'mtproto' | 'web' | 'forward' };
  warnings: string[];
  block_count: number;
  media_count: number;
}

/** GET /api/imports/forward/{id} */
interface TelegramForwardImport {
  id: string;
  expires_at: number;
  state: 'waiting' | 'armed' | 'processing' | 'done' | 'failed' | 'expired';
  result: TelegramImportResult | null;
  error: string | null;
}

interface SpecField {
  name: string;
  types: string[];
  required: boolean;
  description: string;
  enum?: string[] | null;
  range?: [number, number] | null;
}

interface SpecType {
  description: string;
  fields: SpecField[];
  subtypes: string[];
  /** the field that tells this member of a union from the others, and its value */
  const_field: string | null;
  const: string | null;
  href?: string | null;
}

/** `post` in GET /api/spec: the studio's post and the method parameters behind its options and poll_after */
interface SpecPost {
  root: string;
  types: Record<string, { description: string; fields: SpecField[] }>;
}

interface SpecMethod {
  description: string;
  fields: SpecField[];
  href?: string | null;
}

/** GET /api/spec */
interface SpecSummary {
  version: string;
  release_date: string | null;
  changelog: string | null;
  source: string;
  loaded_at: number;
  types: Record<string, SpecType>;
  methods: Record<string, SpecMethod>;
  changes: Json[];
  gaps: SpecGap[];
  last_check: { at: number; error: string | null } | null;
  post: SpecPost;
  /** where Telegram refuses a field or a type (spec/chats.py): `rules` by "Type.field", "Type",
   * "sendRichMessage.parameter"; `places` the words for a chat type ("в канале"; "forum" for a forum supergroup) */
  chat_rules: { rules: Record<string, ChatRule>; places: Record<string, string> };
}

/** A rule of `chat_rules.rules` in GET /api/spec */
interface ChatRule {
  /** Chat.type values (and "ephemeral", a rich reply to a press) Telegram refuses it in: the sentence of the
   * description that says so, Telegram's answer when it was measured, and both in words (`text`) */
  refused: Record<string, { why: string | null; answer: string | null; text: string }>;
  /** the only method it works in (the thinking block: sendRichMessageDraft), so in no post; `why` says so */
  only_in: string | null;
  why: string | null;
  /** the way out, written by hand (a direct link to a Mini App instead of web_app) */
  hint: string | null;
}

/** What labels.js or render.js does not know yet (spec/coverage.py) */
interface SpecGap {
  part: 'labels' | 'preview';
  file: string;
  where: string;
  kind: 'block' | 'inline';
  name: string;
  type: string;
}

/** GET /api/updates, and `updates` in /api/config */
interface UpdatesStatus {
  enabled: boolean;
  mode: 'polling' | 'webhook';
  state: 'starting' | 'ok' | 'off' | 'no_token' | 'conflict' | 'error';
  error: string | null;
  last_at: number | null;
  handled: number;
}

/** GET /api/config */
interface StudioConfig {
  public?: boolean;
  bot_username?: string | null;
  default_chat?: string | null;
  preview_chat?: string | null;
  token_set?: boolean;
  api_base?: string | null;
  spec_version?: string;
  auth?: boolean;
  updates?: UpdatesStatus;
  kinds?: Record<string, MessageKind>;
}

/** A kind of message in /api/config `kinds` (services/kinds.py) */
interface MessageKind {
  label: string;
  badge: string | null;
  editable: boolean;
  stoppable: boolean;
  media: boolean;
}

/** GET /api/drafts row */
interface DraftRow {
  id: string;
  title: string;
  updated_at: number;
  /** the post this draft edits, if any */
  post_id: string | null;
  /** grows with every save: the editor notices a draft changed elsewhere */
  version: number;
  /** the channels the draft is meant for; null: the default channel */
  chat_ids: string[] | null;
}

/** a field of a post template: in the post it stands as {{key}} */
interface TemplateField {
  key: string;
  label: string;
  type: 'text' | 'multiline' | 'url';
  required: boolean;
  default: string;
}

/** GET /api/templates item; GET /api/templates/{id} adds `payload`, the post with {{key}} in it */
interface TemplateRow {
  id: string;
  title: string;
  fields: TemplateField[];
  /** grows with every save: an edit, a delete or a fill based on an older one gets 409 */
  version: number;
  created_at: number;
  updated_at: number;
  payload?: Json;
}

/** GET /api/channels item */
interface Channel {
  chat_id: string;
  chat_num?: number | null;
  title: string | null;
  username?: string | null;
  type?: string | null;
  is_forum: boolean;
  topics: { id: number; name: string }[];
  can_post?: number | null;
  rights?: Json | null;
  checked_at: number | null;
  is_default: boolean;
}

/** GET /api/suggested-posts item */
interface SuggestedPost {
  id: string;
  chat_num: number;
  message_id: number;
  channel_title: string | null;
  author: string | null;
  topic_id: number | null;
  message: Json;
  text: string;
  media: string[];
  state: string;
  price_amount: number | null;
  price_currency: string | null;
  send_date: number | null;
  comment: string | null;
  actor: string | null;
  received_at: number;
  decided_at: number | null;
}

/** GET /api/invite-links item */
interface InviteLink {
  invite_link: string;
  chat_id: string;
  name: string | null;
  kind: 'invite' | 'subscription';
  expire_date: number | null;
  member_limit: number | null;
  creates_join_request: number;
  subscription_period: number | null;
  subscription_price: number | null;
  is_revoked: number;
  joined: number;
  requests: number;
  last_join_at: number | null;
  actor: string | null;
  created_at: number;
}

/** GET /api/posts/{id}/versions item */
interface PostVersion {
  id: number;
  payload: Json;
  replaced_at: number;
  actor: string | null;
}

/** GET /api/posts row */
interface PostRow {
  id: string;
  draft_id: string | null;
  chat_id: string;
  message_id: number | null;
  status: string;
  scheduled_at: number | null;
  sent_at: number | null;
  error: string | null;
  created_at: number;
  kind: string;
  actor: string | null;
  /** +1 on every replacement of the content or the buttons: «Обновить пост» sends it as post_version */
  version: number;
  pinned: boolean;
  /** a failed send that may be in the channel all the same (no answer, a stop or a crash mid-send): stays until removed */
  outcome_unknown: boolean;
  comment_status: string | null;
  /** with comment_status "sending": true while the request is under way; false when nobody sends it any more (Telegram
   * may have it, the record is missing, e.g. a database that refused it); null when this process is a standby and
   * cannot tell (the lease holder sends comments). False for any other comment_status */
  comment_in_flight: boolean | null;
  reactions: Json | null;
  clicks: Json | null;
  /** rich replies to button presses that did not go (Telegram refused, no answer, too late), by callback_data */
  reply_errors: Record<string, { n: number; error: string; at: number }> | null;
  /** callback_data → the text on its button */
  buttons: Record<string, string>;
  poll_results: Json | null;
  poll_question: string | null;
  /** first line of the text (or the poll question), empty when the post has none */
  title: string;
  /** /media/<name> of the first uploaded photo */
  thumb: string | null;
  /** t.me link to the message */
  link: string | null;
}

/** POST /api/widget/validate response (services/widgets.rs) */
interface WidgetReport {
  ok: boolean;
  errors: string[];
  warnings: string[];
  /** source bytes, UTF-8 */
  size_raw: number;
  /** bytes after zlib level 9 */
  size_compressed: number;
}

/** GET /api/media item */
interface Upload {
  name: string;
  kind: string;
  url: string;
  ref: string;
  size: number;
  /** a draft or a live post mentions the file */
  used: boolean;
}

type ButtonRow = Json[];

/** POST /api/preview-send `parsed`: the post as Telegram's parser understood it; the editor adds `key` */
interface ParsedPost {
  /** rich_message the blocks belong to, as JSON: the post has not changed while it equals the current one */
  key: string;
  /** RichBlock* exactly as sendRichMessage returned them */
  blocks: Json[];
  /** the same blocks as InputRichBlock*, ready to send again; media reuse Telegram's file_id */
  input_blocks: Json[];
}

/** <rs-parse-check>: preview compared with Telegram's blocks; mine/theirs are outlines around the first difference */
interface ParseCheck {
  status: 'stale' | 'same' | 'diff';
  mine?: string;
  theirs?: string;
}

/** Editor state (state.js) */
interface EditorState {
  spec: SpecSummary;
  config: StudioConfig;
  draftId: string | null;
  /** version of the open draft as the server last confirmed it; a save based on another one gets 409 */
  draftVersion: number | null;
  /** a 409 dialog is open: autosave waits for the person's choice */
  conflict: boolean;
  format: 'blocks' | 'html' | 'markdown';
  mode: string;
  buf: { blocks: Json[]; html: string; markdown: string };
    formatSnapshot: Record<string, string> | null;
    formatBackup: Json | null;
    legacyFormats: boolean;
  importMeta: Json;

  /** Unapplied JSON source is saved with the draft, including syntax errors; it never becomes the sent post implicitly. */
  pendingJson: string | null;
  json: { text: string; base: string } | null;
  notes: Json;
  unknown: Json;
  media: Json[];
  flags: Json;
  keyboard: ButtonRow[];
  options: Json;
  /** comment: the HTML of the first comment in the form, commentBase: the comment it came in (its other fields go with
   * the HTML), commentObj: a comment that is not HTML, kept as it came; callbacks: every answer as it came, of the
   * buttons in the text too; poll: sendPoll fields of the poll sent right after the post (post.poll_after); markup:
   * reply_markup besides inline_keyboard, as it came */
  extra: { pin: boolean; comment: string; commentBase: Json; commentObj: Json | null; callbacks: Record<string, string | Json>; poll: Json | null; markup: Json };
  updates: UpdatesStatus | null;
  uploads: Upload[];
  posts: PostRow[];
  drafts: DraftRow[];
  channels: Channel[];
  /** the chat the forum topic in options was chosen for; another chat drops the topic */
  topicChat?: string;
}

/** one entry of GET /api/health `checks` */
interface HealthCheck {
  name: string;
  status: 'ok' | 'warn' | 'fail';
  summary: string;
  action?: string;
  details?: Json;
}

/** GET /api/health */
interface HealthReport {
  status: 'ok' | 'warn' | 'fail';
  version: string;
  bot_api: string;
  time: string;
  checked_at: number;
  problems: string[];
  checks: HealthCheck[];
  metrics: Json;
  recent_problems: Json[];
}

/** Personal credentials: only metadata survives the one-time creation response. */
interface AccountToken {
  id: string;
  name: string;
  level: 'read' | 'draft' | 'publish';
  created_at: number;
  expires_at: number | null;
  last_used_at: number | null;
  revoked_at: number | null;
}
interface AccountTokensResponse { tokens: AccountToken[]; mcp_url: string; }
interface AccountTokenCreated { token: AccountToken; secret: string; }
