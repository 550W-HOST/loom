import { stopStack } from "./stack.js";

export default async function globalTeardown(): Promise<void> {
  stopStack();
}
