import assert from "node:assert/strict";
import test from "node:test";
import { renderToStaticMarkup } from "react-dom/server";

import { SettingsPanel, type ProviderSettingsSnapshot } from "./index.tsx";

const snapshot: ProviderSettingsSnapshot = {
  settings: { baseUrl: "https://api.deepseek.com/v1", model: "deepseek-chat", fetchAllow: [], compactMode: false },
  hasApiKey: true,
  secureStorageAvailable: true,
};

test("settings view exposes only key presence and keeps fetch access closed by default", () => {
  const markup = renderToStaticMarkup(
    <SettingsPanel snapshot={snapshot} busy={false} message={null} onSave={() => Promise.resolve()} onStoreKey={() => Promise.resolve()} onDeleteKey={() => Promise.resolve()} onProbe={() => Promise.resolve("connected")} />,
  );
  assert.ok(markup.includes("已保存密钥（明文不可读取）"));
  assert.ok(markup.includes("默认不允许任何主机"));
  assert.ok(!markup.includes("value=\"secret"));
  assert.ok(!markup.includes("sk-"));
});

test("secure storage absence disables credential operations", () => {
  const markup = renderToStaticMarkup(
    <SettingsPanel snapshot={{ ...snapshot, hasApiKey: false, secureStorageAvailable: false }} busy={false} message={null} onSave={() => Promise.resolve()} onStoreKey={() => Promise.resolve()} onDeleteKey={() => Promise.resolve()} onProbe={() => Promise.resolve("rejected")} />,
  );
  assert.ok(markup.includes("系统密钥库不可用"));
  assert.ok((markup.match(/disabled/g) ?? []).length >= 4);
});
