import { startStack } from "./stack.js";

/**
 * Starts the stack once for the whole run.
 *
 * Playwright calls this in its own process, which is why the URL travels
 * through the state file rather than through a return value: a test file
 * cannot import a value that only exists here.
 */
export default async function globalSetup(): Promise<void> {
  const state = await startStack();
  console.log(`loom e2e: server and daemon up at ${state.baseURL}`);
  console.log(`loom e2e: stack logs in ${state.logs.server} and ${state.logs.daemon}`);
}
