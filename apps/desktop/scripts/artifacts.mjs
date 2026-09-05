#!/usr/bin/env node
/**
 * TASK-909：发布产物清单生成——SHA256 校验和 + 轻量 SBOM。
 * 用法：node scripts/artifacts.mjs <安装包路径...>
 * 输出：<安装包目录>/SHA256SUMS 与 sbom.json（Cargo.lock + package-lock 组件清单）。
 */
import { createHash } from "node:crypto";
import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { basename, dirname, join, resolve } from "node:path";

const inputs = process.argv.slice(2);
if (inputs.length === 0) {
  console.error("usage: node scripts/artifacts.mjs <artifact...>");
  process.exit(1);
}

const outDir = dirname(resolve(inputs[0]));
const lines = [];
for (const artifact of inputs) {
  const content = readFileSync(artifact);
  const digest = createHash("sha256").update(content).digest("hex");
  lines.push(`${digest}  ${basename(artifact)}`);
}
writeFileSync(join(outDir, "SHA256SUMS"), lines.join("\n") + "\n");

const components = [];
const readTomlPackages = (text) => {
  const pairs = [];
  let name = null;
  for (const line of text.split(/\r?\n/)) {
    const nameMatch = line.match(/^name = "(.+)"$/);
    const versionMatch = line.match(/^version = "(.+)"$/);
    if (nameMatch) name = nameMatch[1];
    else if (versionMatch && name !== null) {
      pairs.push([name, versionMatch[1]]);
      name = null;
    }
  }
  return pairs;
};
const readNpmPackages = (text) => {
  const lock = JSON.parse(text);
  return Object.entries(lock.packages ?? {})
    .filter(([key]) => key !== "")
    .map(([key, info]) => [key.replace("node_modules/", ""), info.version ?? ""]);
};

const cargoPath = join(process.cwd(), "../../Cargo.lock");
if (existsSync(cargoPath)) readLock(cargoPath, "cargo", readTomlPackages);
const npmPath = join(process.cwd(), "package-lock.json");
if (existsSync(npmPath)) readLock(npmPath, "npm", readNpmPackages);

const sbom = {
  generator: "ideal-harness 0.9.0 lightweight SBOM (TASK-909)",
  generatedAt: new Date().toISOString(),
  components,
};
writeFileSync(join(outDir, "sbom.json"), JSON.stringify(sbom, null, 2) + "\n");
console.log(`SHA256SUMS + sbom.json (${components.length} components) written to ${outDir}`);

function readLock(path, source, map) {
  if (!existsSync(path)) return;
  for (const [name, version] of map(readFileSync(path, "utf8"))) {
    components.push({ name, version, source });
  }
}
