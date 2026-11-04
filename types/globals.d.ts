// Globals the classic scripts put on window for the editor modules.
interface RenderOptions {
  mediaBase?: string;
  channelTitle?: string;
  views?: string | number;
  // id кастомного эмодзи -> 'video' | 'tgs': редактор знает это из настроенных паков
  emojiKinds?: Record<string, string>;
}
declare const RichRender: {
  setLocale(locale: string): void;
  render(post: object, opts?: RenderOptions): HTMLElement;
  renderRT(rt: unknown, ctx: object): Node;
  renderBlocks(blocks: unknown[], ctx: object): Node;
  formatTime(unix: number, fmt?: string): string;
  mediaSrc(media: unknown, ctx: object): string | null;
  safeUrl(url: string): string | null;
  outline(post: object): string;
  markdownHtml(source: string): string;
};
interface Window {
  RichRender: typeof RichRender;
  // vendored libraries the preview uses when present
  katex?: { render(expr: string, el: HTMLElement, opts?: object): void };
  markdownit?: (opts?: object) => any;
  // snippets.js: snippet groups for the html and markdown modes
  SNIPPETS: Record<string, Record<string, any>[]>;
  // vendor/tlottie/tlottie.js: играние .tgs-анимаций в предпросмотре
  Tlottie?: {
    play(canvas: HTMLCanvasElement, bytes: Uint8Array): Promise<void>;
    stop(canvas: HTMLCanvasElement): void;
    loaded(): boolean;
  };
}
