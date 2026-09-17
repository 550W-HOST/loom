import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { extname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const uiDir = resolve(fileURLToPath(new URL("..", import.meta.url)));
const port = Number(process.env.LOOM_UI_PORT ?? 5173);
const contentTypes = {
  ".css": "text/css; charset=utf-8",
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".map": "application/json",
};

const server = createServer(async (request, response) => {
  const requestPath = new URL(request.url ?? "/", "http://localhost").pathname;
  if (
    requestPath.startsWith("/api/") ||
    requestPath === "/ws" ||
    requestPath === "/internal/ws"
  ) {
    response.writeHead(404, { "content-type": "text/plain; charset=utf-8" });
    response.end("Use LOOM_UI_PROXY on loom-server for API and WebSocket traffic.\n");
    return;
  }

  const relativePath = requestPath === "/" ? "index.html" : requestPath.slice(1);
  const filePath = resolve(uiDir, relativePath);
  if (!filePath.startsWith(`${uiDir}/`)) {
    response.writeHead(404);
    response.end();
    return;
  }

  try {
    const body = await readFile(filePath);
    response.writeHead(200, {
      "cache-control": "no-cache",
      "content-type": contentTypes[extname(filePath)] ?? "application/octet-stream",
    });
    response.end(body);
  } catch {
    if (extname(requestPath) === "") {
      try {
        const body = await readFile(resolve(uiDir, "index.html"));
        response.writeHead(200, {
          "cache-control": "no-cache",
          "content-type": "text/html; charset=utf-8",
        });
        response.end(body);
        return;
      } catch {
        // Fall through to the honest 404 below.
      }
    }
    response.writeHead(404);
    response.end();
  }
});

server.listen(port, "127.0.0.1", () => {
  console.log(`loom-ui dev server listening on http://127.0.0.1:${port}`);
});
