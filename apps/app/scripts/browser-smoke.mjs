#!/usr/bin/env node

import { createServer } from "node:http";
import { mkdir, readFile } from "node:fs/promises";
import { extname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const appRoot = resolve(fileURLToPath(new URL("..", import.meta.url)));
const distRoot = resolve(appRoot, "dist");
const screenshotRoot = resolve(
  process.env.LOOM_PRODUCT_APP_SCREENSHOTS ?? resolve(appRoot, "../../artifacts/product-app"),
);
const contentTypes = {
  ".css": "text/css; charset=utf-8",
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".map": "application/json",
  ".png": "image/png",
  ".woff2": "font/woff2",
};

function safePath(pathname) {
  const relativePath = pathname === "/" ? "index.html" : pathname.slice(1);
  const filePath = resolve(distRoot, relativePath);
  return filePath.startsWith(`${distRoot}/`) ? filePath : null;
}

function createStaticServer() {
  return createServer(async (request, response) => {
    const pathname = new URL(request.url ?? "/", "http://localhost").pathname;
    const filePath = safePath(pathname);
    if (!filePath) {
      response.writeHead(404);
      response.end();
      return;
    }
    try {
      const body = await readFile(filePath);
      response.writeHead(200, { "content-type": contentTypes[extname(filePath)] ?? "application/octet-stream" });
      response.end(body);
    } catch {
      if (extname(pathname) === "") {
        const body = await readFile(resolve(distRoot, "index.html"));
        response.writeHead(200, { "content-type": "text/html; charset=utf-8" });
        response.end(body);
        return;
      }
      response.writeHead(404);
      response.end();
    }
  });
}

async function listen(server) {
  await new Promise((resolvePromise, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolvePromise);
  });
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("smoke server did not expose a port");
  return `http://127.0.0.1:${address.port}/`;
}

async function assertShell(page, label) {
  await page.waitForSelector('[data-testid="product-app"]');
  const bodyText = await page.locator("body").textContent();
  for (const expected of ["loom", "What are you working on?", "No threads yet", "No projects connected"]) {
    if (!bodyText?.includes(expected)) throw new Error(`${label}: missing text ${expected}`);
  }
  const dimensions = await page.evaluate(() => ({
    scrollWidth: document.documentElement.scrollWidth,
    clientWidth: document.documentElement.clientWidth,
  }));
  if (dimensions.scrollWidth > dimensions.clientWidth + 1) {
    throw new Error(`${label}: horizontal overflow ${JSON.stringify(dimensions)}`);
  }
}

function screenshotPath(name) {
  return resolve(screenshotRoot, `${name}.png`);
}

const server = createStaticServer();
const browser = await chromium.launch({ headless: true });
await mkdir(screenshotRoot, { recursive: true });

try {
  const baseUrl = await listen(server);
  const desktop = await browser.newPage({ viewport: { width: 1440, height: 960 } });
  await desktop.goto(baseUrl, { waitUntil: "load" });
  await assertShell(desktop, "desktop");
  await desktop.screenshot({ path: screenshotPath("product-shell-desktop-light"), fullPage: true });
  await desktop.getByTitle("Use dark theme").click();
  await desktop.waitForFunction(() => document.documentElement.dataset.theme === "dark");
  await desktop.screenshot({ path: screenshotPath("product-shell-desktop-dark"), fullPage: true });
  await desktop.getByRole("button", { name: "Open command menu" }).click();
  await desktop.getByRole("dialog", { name: "Jump to" }).waitFor();
  await desktop.screenshot({ path: screenshotPath("product-shell-command-menu"), fullPage: true });

  const mobile = await browser.newPage({ viewport: { width: 390, height: 844 } });
  await mobile.goto(baseUrl, { waitUntil: "load" });
  await assertShell(mobile, "mobile");
  await mobile.screenshot({ path: screenshotPath("product-shell-mobile"), fullPage: true });
  await mobile.getByRole("button", { name: "Open navigation" }).click();
  await mobile.locator(".sidebar-layer--open").waitFor();
  await mobile.waitForTimeout(250);
  await mobile.screenshot({ path: screenshotPath("product-shell-mobile-navigation"), fullPage: true });
  console.log(`browser smoke OK: screenshots in ${screenshotRoot}`);
} finally {
  await browser.close();
  await new Promise((resolvePromise) => server.close(resolvePromise));
}
