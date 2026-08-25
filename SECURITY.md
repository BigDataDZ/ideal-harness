# Security Policy

## 支持版本

| 版本 | 状态 |
|---|---|
| 0.1.x | 原型期，仅接受高危问题报告 |

## 报告漏洞

**不要使用公开 Issue 报告安全漏洞。**

本项目是安全敏感组件（沙箱 / 审批 / 进程隔离），请通过 GitHub
Private Vulnerability Reporting（仓库 Security 标签页）私密报告。

报告请包含：
- 影响的模块（如 `sandbox-policy` / `approval`）与对应决策编号（DESIGN-DECISIONS D1~D13）
- 复现步骤或 PoC
- 影响评估（能否逃逸沙箱 / 绕过审批 / 数据外传）

## 特别关注的攻击面

以下类别的问题会被优先处理：

1. **沙箱逃逸**：绕过 `ensures_writable` 词法栅栏（符号链接别名、路径规范化差异）
2. **提权滥用**：`approve_escalation` 的收窄放行、无审批器放行
3. **审计缺失**：任何"静默失败不留 Event"的路径
4. **配对破坏**：上下文裁剪拆散 tool_call/tool_result 导致模型状态错乱
5. **错误信道注入**：诱导控制流解析 message 字符串（本仓库以红线禁止，发现违例请报告）

## 响应目标

- 72 小时内确认收到
- 高危（逃逸/绕过）：7 天内修复或缓解
- 修复随下一个 minor 版本发布，并在 CHANGELOG 中致谢报告者（除非要求匿名）
