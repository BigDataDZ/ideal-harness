import ReactMarkdown from "react-markdown";

const MAX_RENDERED_MARKDOWN = 65_536;

export function SafeMarkdown({ children }: { children: string }) {
  const prepared = prepareMarkdown(children);
  return (
    <div className="safe-markdown">
      <ReactMarkdown
        skipHtml
        urlTransform={safeUrlTransform}
        components={{
          a: ({ href, children: linkChildren }) =>
            href ? <a href={href} target="_blank" rel="noreferrer noopener">{linkChildren}</a> : <span>{linkChildren}</span>,
          img: ({ alt }) => <span className="blocked-image">[图片已阻止{alt ? `：${alt}` : ""}]</span>,
        }}
      >
        {prepared.markdown}
      </ReactMarkdown>
      {prepared.truncated ? <p className="markdown-truncated">消息过长，仅渲染前 {MAX_RENDERED_MARKDOWN.toLocaleString()} 个字符。</p> : null}
    </div>
  );
}

export function prepareMarkdown(markdown: string): { markdown: string; truncated: boolean } {
  if (markdown.length <= MAX_RENDERED_MARKDOWN) return { markdown, truncated: false };
  return { markdown: markdown.slice(0, MAX_RENDERED_MARKDOWN), truncated: true };
}

export function safeUrlTransform(url: string): string {
  const normalized = url.trim();
  if (normalized === "" || /[\u0000-\u001F\u007F]/.test(normalized)) return "";
  if (normalized.startsWith("#")) return normalized;
  if ((normalized.startsWith("/") && !normalized.startsWith("//")) || normalized.startsWith("./") || normalized.startsWith("../")) {
    return normalized;
  }
  try {
    const parsed = new URL(normalized);
    return parsed.protocol === "https:" || parsed.protocol === "http:" ? normalized : "";
  } catch {
    return "";
  }
}
