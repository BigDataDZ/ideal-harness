import { useDeferredValue, useMemo, useState, type FormEvent } from "react";

import type { ProjectionSnapshot } from "../../lib/projection/index.ts";
import { projectConversation } from "./conversation.ts";
import { SafeMarkdown } from "./SafeMarkdown.tsx";
import type { ChatMessage, ConversationItem, ToolCardView } from "./types.ts";

const MAX_RENDERED_ITEMS = 300;

interface ChatPanelProps {
  snapshot: ProjectionSnapshot | null;
  startAvailable?: boolean;
  onSend: (input: string) => void;
  onSteer: (turnId: number, input: string) => void;
  onCancel: (turnId: number) => void;
  onResume: () => void;
}

export function ChatPanel({ snapshot, startAvailable = true, onSend, onSteer, onCancel, onResume }: ChatPanelProps) {
  const deferredSnapshot = useDeferredValue(snapshot);
  const view = useMemo(
    () => (deferredSnapshot ? projectConversation(deferredSnapshot) : null),
    [deferredSnapshot],
  );
  const [input, setInput] = useState("");

  if (!view) {
    return (
      <section className="chat-shell chat-shell--empty" aria-labelledby="chat-title">
        <p className="panel-kicker">CONVERSATION</p>
        <h2 id="chat-title">选择会话后开始</h2>
        <p>消息、流式文本和工具结果只从 Event 投影，不在客户端猜测执行结果。</p>
      </section>
    );
  }

  const visibleItems = view.items.slice(-MAX_RENDERED_ITEMS);
  const omitted = view.items.length - visibleItems.length;
  const submit = (event: FormEvent) => {
    event.preventDefault();
    const normalized = input.trim();
    if (!normalized) return;
    if (view.canSteer && view.activeTurnId !== null) onSteer(view.activeTurnId, normalized);
    else if (view.canSend && startAvailable) onSend(normalized);
    else return;
    setInput("");
  };

  return (
    <section className="chat-shell" aria-labelledby="chat-title">
      <header className="chat-header">
        <div><p className="panel-kicker">CONVERSATION</p><h2 id="chat-title">{view.sessionId}</h2></div>
        <div className="chat-controls">
          {view.canResume ? <button type="button" onClick={onResume}>Resume</button> : null}
          {view.canCancel && view.activeTurnId !== null ? (
            <button type="button" className="danger-button" onClick={() => onCancel(view.activeTurnId!)}>取消 Turn</button>
          ) : null}
        </div>
      </header>

      {view.integrityIssues.length > 0 ? (
        <div className="chat-integrity" role="alert">
          <strong>事件配对不完整，写操作已关闭</strong>
          {view.integrityIssues.map((issue) => <code key={issue}>{issue}</code>)}
        </div>
      ) : null}
      {omitted > 0 ? <p className="render-window" role="status">为保持流畅，已折叠较早的 {omitted} 项；权威事件未被删除。</p> : null}

      <div className="conversation-list" aria-live="polite" aria-label="对话记录">
        {visibleItems.length === 0 ? <p className="muted">此会话还没有消息。</p> : visibleItems.map((item) => <ConversationItemView key={`${item.kind}-${item.seq}`} item={item} />)}
      </div>

      <form className="chat-composer" onSubmit={submit}>
        <label htmlFor="chat-input" className="sr-only">输入消息</label>
        <textarea
          id="chat-input"
          value={input}
          onChange={(event) => setInput(event.currentTarget.value)}
          placeholder={view.canSteer ? "输入 steer，在下一采样边界生效…" : startAvailable ? "输入消息…" : "等待 TASK-908 配置 Provider…"}
          disabled={(!view.canSend || !startAvailable) && !view.canSteer}
          rows={3}
        />
        <div>
          <span>{view.canSteer ? `Steer turn #${view.activeTurnId}` : view.connection === "connected" ? "新 Turn" : "等待重连"}</span>
          <button type="submit" disabled={input.trim() === "" || ((!view.canSend || !startAvailable) && !view.canSteer)}>
            {view.canSteer ? "Steer" : "发送"}
          </button>
        </div>
      </form>
    </section>
  );
}

function ConversationItemView({ item }: { item: ConversationItem }) {
  return item.kind === "message" ? <MessageBubble message={item.message} /> : <ToolCard tool={item.tool} />;
}

function MessageBubble({ message }: { message: ChatMessage }) {
  return (
    <article className={`message-bubble message-bubble--${message.role}`} data-state={message.state}>
      <header><span>{message.role === "assistant" ? "Agent" : message.role === "user" ? "You" : "System"}</span><code>#{message.seq}</code></header>
      <SafeMarkdown>{message.markdown || (message.state === "streaming" ? "正在生成…" : "无文本内容")}</SafeMarkdown>
      {message.state !== "complete" ? <p className={`message-state message-state--${message.state}`}>{messageStateLabel(message.state)}</p> : null}
    </article>
  );
}

function ToolCard({ tool }: { tool: ToolCardView }) {
  return (
    <article className={`tool-card tool-card--${tool.status}`}>
      <header><div><span>TOOL</span><strong>{tool.tool}</strong></div><StatusBadge status={tool.status} /></header>
      <dl className="tool-facts">
        <div><dt>call id</dt><dd>{tool.callId}</dd></div>
        <div><dt>审计</dt><dd>{tool.audit.join(" → ")}</dd></div>
        <div><dt>耗时</dt><dd>{tool.eventSpan === null ? "等待结果" : `未记录时间 · event span ${tool.eventSpan}`}</dd></div>
      </dl>
      <details><summary>参数</summary><pre>{safeJson(tool.args)}</pre></details>
      {tool.error ? <div className="tool-error" role="alert"><code>{tool.error.code}</code><p>{tool.error.message}</p></div> : null}
      {tool.resultPreview ? <pre className="tool-result">{tool.resultPreview}</pre> : null}
    </article>
  );
}

function StatusBadge({ status }: { status: ToolCardView["status"] }) {
  const labels: Record<ToolCardView["status"], string> = {
    running: "执行中", success: "成功", rejected: "已拒绝", failed: "失败", timed_out: "超时", cancelled: "已取消",
  };
  return <span className={`tool-status tool-status--${status}`}>{labels[status]}</span>;
}

function messageStateLabel(state: ChatMessage["state"]): string {
  if (state === "streaming") return "流式接收中";
  if (state === "interrupted") return "流已中断，可 Resume 补洞";
  if (state === "queued") return "已排队，将在下一采样边界生效";
  return "";
}

function safeJson(value: unknown): string {
  try { return JSON.stringify(value, null, 2) ?? "null"; } catch { return "[无法序列化的参数]"; }
}
