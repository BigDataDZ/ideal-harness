import { expect, test, type Page } from "@playwright/test";

/**
 * TASK-909：scripted-provider 桌面 E2E。
 * 注入 __TAURI_INTERNALS__ mock，以脚本化事件帧驱动真实前端，
 * 覆盖「新建会话 → 对话 → 工具卡 → 审批 → 文件 Diff → 完成 → 重启恢复」。
 */

interface Frame {
  session_id: string;
  connection_generation: number;
  record: { seq: number; event: Record<string, unknown> };
}

const HOST = { operation: "desktop_status", generation: 3, permissionEpoch: 5, turnId: null };

function frame(seq: number, event: Record<string, unknown>): Frame {
  return { session_id: "demo", connection_generation: 3, record: { seq, event } };
}

const FRAMES: Frame[] = [
  frame(0, { type: "turn_started", turn_id: 0 }),
  frame(1, { type: "user_message", text: "修复 add 函数" }),
  frame(2, {
    type: "tool_call_requested",
    call_id: "call_1",
    tool: "fs_read",
    args: { path: "src/lib.rs" },
  }),
  frame(3, {
    type: "tool_result_added",
    call_id: "call_1",
    outcome: {
      success: {
        value: {
          path: "src/lib.rs",
          content: "pub fn add(a: i32) -> i32 {\n    a + 1\n}\n",
          hash: "fnv1a:9d45e31777c0c71f",
        },
      },
    },
  }),
  frame(4, {
    type: "tool_call_requested",
    call_id: "call_2",
    tool: "fs_edit",
    args: {
      path: "src/lib.rs",
      old_string: "a + 1",
      new_string: "a + 2",
      expected_hash: "fnv1a:9d45e31777c0c71f",
    },
  }),
  frame(5, {
    type: "tool_result_added",
    call_id: "call_2",
    outcome: { success: { value: { path: "src/lib.rs", replacements: 1 } } },
  }),
  frame(6, {
    type: "approval_decided",
    call_id: "call_2",
    approved: true,
  }),
  frame(7, { type: "assistant_message", text: "修复完成，测试通过" }),
  frame(8, { type: "turn_completed", turn_id: 0 }),
];

async function installBridge(page: Page) {
  await page.addInitScript(
    ([frames]) => {
      const handler = (command: string) => {
        switch (command) {
          case "desktop_status":
            return Promise.resolve({
              operation: "desktop_status",
              generation: 3,
              permissionEpoch: 5,
              turnId: null,
            });
          case "get_provider_settings":
            return Promise.resolve({
              settings: {
                baseUrl: "https://provider.example/v1",
                model: "demo-model",
                fetchAllow: [],
                compactMode: false,
              },
              hasApiKey: true,
              secureStorageAvailable: true,
            });
          case "session_operation":
            return Promise.resolve({
              sessionId: "demo",
              eventCount: frames.length,
              generation: 3,
            });
          case "session_event_frames":
            return Promise.resolve(frames);
          default:
            return Promise.resolve({
              operation: command,
              generation: 3,
              permissionEpoch: 5,
              turnId: null,
            });
        }
      };
      Object.defineProperty(window, "__TAURI_INTERNALS__", {
        value: {
          metadata: { currentWindow: { label: "main" }, currentWebview: { label: "main" } },
          plugins: {},
          transformCallback: (callback: unknown) => callback,
          invoke: (command: string) => handler(command) as Promise<unknown>,
        },
      });
    },
    [FRAMES],
  );
}

test.describe("TASK-909 桌面端到端（scripted provider）", () => {
  test.beforeEach(async ({ page }) => {
    page.on("pageerror", (error) => console.log("PAGE-ERROR>>>", error.message));
    page.on("console", (message) => {
      if (message.type() === "error") console.log("CONSOLE-ERROR>>>", message.text());
    });
    await installBridge(page);
  });

  test("新建会话后完成对话、工具卡、审批与 Diff 的全链投影", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByText("GEN 3")).toBeVisible();

    await page.getByPlaceholder("新会话 ID").fill("demo");
    await page.getByRole("button", { name: "新建" }).click();
    // 宿主回执：批量事件已记录（列表不做乐观更新）
    await expect(page.getByText("宿主已记录 9 个事件")).toBeVisible();

    await expect(page.getByText("修复 add 函数")).toBeVisible();
    await expect(page.getByText("修复完成，测试通过")).toBeVisible();
    await expect(page.getByText("fs_read").first()).toBeVisible();
    await expect(page.getByText("fs_edit").first()).toBeVisible();

    await page.getByRole("button", { name: "审批" }).click();
    await expect(page.getByText("已批准")).toBeVisible();
    await expect(page.getByText("call_2")).toBeVisible();

    await page.getByRole("button", { name: "工作区" }).click();
    await expect(page.getByText("src/lib.rs").first()).toBeVisible();
    await expect(page.getByText("与最近 fs_read hash 一致")).toBeVisible();
    await expect(page.locator(".diff-view").getByText("+ a + 2")).toBeVisible();
  });

  test("重启（reload）后由事件流重建相同状态", async ({ page }) => {
    await page.goto("/");
    await page.getByPlaceholder("新会话 ID").fill("demo");
    await page.getByRole("button", { name: "新建" }).click();
    await expect(page.getByText("修复完成，测试通过")).toBeVisible();

    await page.reload();
    await expect(page.getByText("GEN 3")).toBeVisible();
    await page.getByPlaceholder("新会话 ID").fill("demo");
    await page.getByRole("button", { name: "新建" }).click();
    await expect(page.getByText("修复完成，测试通过")).toBeVisible();
  });

  test("设置页：Provider 快照按宿主返回渲染", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByText("GEN 3")).toBeVisible();
    await page.getByRole("button", { name: "设置" }).click();
    try {
      await expect(page.getByRole("heading", { name: "Provider 设置" })).toBeVisible({ timeout: 5000 });
    } catch (error) {
      console.log("PAGE-DUMP>>>", (await page.content()).length, JSON.stringify((await page.locator("main").innerHTML()).slice(0, 700)));
      throw error;
    }
    await expect(page.getByLabel("Base URL")).toHaveValue("https://provider.example/v1");
  });
});
