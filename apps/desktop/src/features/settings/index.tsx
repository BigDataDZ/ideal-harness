import { useEffect, useRef, useState, type FormEvent } from "react";

export interface ProviderSettings {
  baseUrl: string;
  model: string;
  fetchAllow: string[];
  compactMode: boolean;
}

export interface ProviderSettingsSnapshot {
  settings: ProviderSettings;
  hasApiKey: boolean;
  secureStorageAvailable: boolean;
  /** 已存 key 的掩码指纹（非密钥材料），用于确认「存的是哪把」。 */
  apiKeyMask?: string;
}

export type ProbeResult =
  | "connected"
  | "authentication_failed"
  | "network_unavailable"
  | "timed_out"
  | "rejected";

interface SettingsPanelProps {
  snapshot: ProviderSettingsSnapshot | null;
  busy: boolean;
  message: string | null;
  onSave(settings: ProviderSettings): Promise<void>;
  onStoreKey(key: string): Promise<void>;
  onDeleteKey(): Promise<void>;
  onProbe(): Promise<ProbeResult>;
}

export function SettingsPanel({
  snapshot,
  busy,
  message,
  onSave,
  onStoreKey,
  onDeleteKey,
  onProbe,
}: SettingsPanelProps) {
  const [baseUrl, setBaseUrl] = useState("");
  const [model, setModel] = useState("");
  const [fetchAllow, setFetchAllow] = useState("");
  const [compactMode, setCompactMode] = useState(false);
  const keyInput = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (!snapshot) return;
    setBaseUrl(snapshot.settings.baseUrl);
    setModel(snapshot.settings.model);
    setFetchAllow(snapshot.settings.fetchAllow.join("\n"));
    setCompactMode(snapshot.settings.compactMode);
  }, [snapshot]);

  const save = (event: FormEvent) => {
    event.preventDefault();
    void onSave({
      baseUrl,
      model,
      fetchAllow: fetchAllow.split(/\r?\n/).map((host) => host.trim()).filter(Boolean),
      compactMode,
    });
  };

  const storeKey = () => {
    const input = keyInput.current;
    if (!input || input.value.trim() === "") return;
    const value = input.value;
    input.value = "";
    // TASK-909 修复：写密钥前先把当前表单的 Base URL/Model 一并保存，
    // 避免写密钥后的设置回读把未保存的表单编辑冲掉。
    void onSave({
      baseUrl,
      model,
      fetchAllow: fetchAllow.split(/\r?\n/).map((host) => host.trim()).filter(Boolean),
      compactMode,
    }).then(() => onStoreKey(value));
  };

  return (
    <section className="settings-shell" aria-labelledby="settings-title">
      <header className="feature-header">
        <div><p className="panel-kicker">LOCAL SECURITY</p><h2 id="settings-title">Provider 设置</h2></div>
        <span className={snapshot?.secureStorageAvailable ? "read-only-badge" : "approval-readiness approval-readiness--invalid"}>
          {snapshot?.secureStorageAvailable ? "系统密钥库可用" : "系统密钥库不可用"}
        </span>
        {snapshot?.apiKeyMask ? (
          <p className="settings-note">已存 key 指纹：{snapshot.apiKeyMask}</p>
        ) : null}
      </header>
      {!snapshot ? <div className="feature-empty"><strong>正在读取本机设置</strong></div> : (
        <div className="settings-grid">
          <form className="settings-card" onSubmit={save}>
            <h3>模型与网络</h3>
            <label>Base URL<input type="url" required value={baseUrl} onChange={(event) => setBaseUrl(event.target.value)} /></label>
            <label>Model<input required maxLength={256} value={model} onChange={(event) => setModel(event.target.value)} /></label>
            <label>Fetch 白名单<textarea value={fetchAllow} onChange={(event) => setFetchAllow(event.target.value)} placeholder="每行一个精确主机名；默认不允许任何主机" /></label>
            <label className="settings-check"><input type="checkbox" checked={compactMode} onChange={(event) => setCompactMode(event.target.checked)} />紧凑界面</label>
            <button type="submit" disabled={busy}>保存并更新安全代际</button>
          </form>
          <div className="settings-card">
            <h3>API Key</h3>
            <p className="settings-note">密钥仅写入操作系统凭据库；保存后前端不能读取或显示明文。</p>
            <label>新密钥<input ref={keyInput} type="password" autoComplete="new-password" disabled={!snapshot.secureStorageAvailable || busy} /></label>
            <div className="settings-actions">
              <button type="button" onClick={storeKey} disabled={!snapshot.secureStorageAvailable || busy}>写入系统密钥库</button>
              <button type="button" className="danger-button" onClick={() => void onDeleteKey()} disabled={!snapshot.hasApiKey || busy}>删除密钥</button>
            </div>
            <p className={snapshot.hasApiKey ? "positive-text" : "warning-text"}>{snapshot.hasApiKey ? "已保存密钥（明文不可读取）" : "尚未保存密钥"}</p>
            <button type="button" onClick={() => void onProbe()} disabled={!snapshot.hasApiKey || busy}>测试连接</button>
          </div>
        </div>
      )}
      {message ? <p className="inline-alert" role="status">{message}</p> : null}
    </section>
  );
}
