/**
 * Compiler negative tests for the typed `apiClient` seam.
 *
 * Every `@ts-expect-error` here asserts that the annotated call does **not**
 * type-check. `tsc` fails the build if the error disappears, so a change that
 * loosens the seam (back to `json?: unknown`, a generic query record, or a
 * `$get` on a POST route) breaks `pnpm typecheck` rather than silently letting
 * a browser-rejected request through.
 *
 * `@ts-expect-error` must be the line immediately above the erroring line, so
 * the directives sit on the property that is wrong rather than on the call.
 *
 * This file is compiled by `tsconfig.type-tests.json`, which is part of the
 * typecheck script; the main `tsconfig.json` excludes test files, so a
 * `@ts-expect-error` in a test file would never be evaluated.
 */

import type {
  CreateThreadRequest,
  ResolvePendingInteractionRequest,
  SendMessageRequest,
  UpdateThreadTabsRequest,
  UpdateUiPreferenceRequest,
} from "@bb/server-contract";
import { apiClient } from "./api-server";
import { loomApiFetch, loomApiJson, resolveLoomApiMethod } from "./loom-http";
import { LOOM_API_ROUTES } from "./loom-api-routes";

// --- A GET route cannot be given a body ------------------------------------

void apiClient.projects[":id"]["branch-options"].$get({
  param: { id: "p1" },
  // @ts-expect-error a GET route takes a query, not a JSON body
  json: { anything: true },
});

void apiClient.projects[":id"]["branch-options"].$get({
  param: { id: "p1" },
  // @ts-expect-error a GET route cannot take multipart either
  formData: new FormData(),
});

// --- A POST route cannot be read, and cannot take a query ------------------

// @ts-expect-error hosts.createJoinCode is POST; there is no $get
void apiClient.hosts["join-codes"].$get({ json: {} });

void apiClient.hosts["join-codes"].$post({
  // @ts-expect-error the join-code route declares a JSON body, not a query
  query: { unused: "x" },
  json: {},
});

void apiClient.hosts["join-codes"].$post({
  // @ts-expect-error multipart where the contract declares JSON
  formData: new FormData(),
});

void apiClient.system["voice-transcription"].$post({
  // @ts-expect-error voice transcription is form-only; a JSON body is wrong
  json: {},
});

// --- Query keys are precise ------------------------------------------------

void apiClient.threads[":id"]["thread-storage"].content.$get({
  param: { id: "t1" },
  // @ts-expect-error `path` is required by the contract query
  query: {},
});

void apiClient.threads[":id"]["thread-storage"].content.$get({
  param: { id: "t1" },
  query: {
    path: "a.ts",
    // @ts-expect-error `notAQueryKey` is not in the contract's query
    notAQueryKey: "x",
  },
});

void apiClient.threads[":id"]["thread-storage"].content.$get({
  param: { id: "t1" },
  // @ts-expect-error `path` must be a string, not a number
  query: { path: 42 },
});

void apiClient.environments[":id"].diff.file.$get({
  param: { id: "e1" },
  // @ts-expect-error `target` is required by the diff-file discriminated union
  query: { path: "a.ts", side: "new" },
});

// --- Path parameters are required and named -------------------------------

// @ts-expect-error `id` is required by the route path
void apiClient.threads[":id"]["thread-storage"].content.$get({});

void apiClient.threads[":id"].worktree.files[":filePath{.+}"].$url({
  // @ts-expect-error `id` and `filePath` are both required by the catch-all route
  param: {},
});

void apiClient.threads[":id"].worktree.files[":filePath{.+}"].$url({
  // @ts-expect-error `wrongName` is not a parameter of this route
  param: { wrongName: "a.ts" },
});

// @ts-expect-error a route with no :param cannot be given a param bag
void apiClient.system.config.$get({ param: { id: "x" } });

// @ts-expect-error `delete` is not a route on hosts
void apiClient.hosts.delete.$url({ param: { id: "h1" } });

// @ts-expect-error an unknown top-level area is not a route
void apiClient.nothing.$get({});

// --- The lowest transport is typed too ------------------------------------

// These are the same rules the seam applies, now enforced on the exported
// transport itself: previously `loomApiFetch` took a generic bag, so an untyped
// caller could put a body on a route-table GET and reach the browser.
void loomApiFetch("threads.worktreeFile", {
  param: { id: "t1", filePath: "a.ts" },
  // @ts-expect-error a GET route declares no JSON body in the contract
  json: { x: 1 },
});

void loomApiFetch("threads.worktreeFile", {
  param: { id: "t1", filePath: "a.ts" },
  // @ts-expect-error a GET route cannot take multipart either
  formData: new FormData(),
});

void loomApiJson("system.config", {
  // @ts-expect-error `system.config` declares no query in the contract
  query: { anything: true },
});

void loomApiFetch("system.config", {
  // @ts-expect-error `system.config` declares no JSON body
  json: {},
});

// @ts-expect-error a JSON route requires its body
void loomApiFetch("hosts.createJoinCode", {});

// @ts-expect-error a JSON route requires its argument bag
void loomApiFetch("hosts.createJoinCode");

// @ts-expect-error loomApiJson enforces the same required body
void loomApiJson("hosts.createJoinCode");

// @ts-expect-error a JSON route cannot be sent multipart instead
void loomApiFetch("hosts.createJoinCode", { formData: new FormData() });

void loomApiFetch("hosts.createJoinCode", {
  // @ts-expect-error the join-code route declares no query
  query: { unused: "x" },
  json: {},
});

void loomApiFetch("threads.storageContent", {
  param: { id: "t1" },
  // @ts-expect-error `path` is required by the contract query
  query: {},
});

// @ts-expect-error a required query cannot be omitted entirely
void loomApiFetch("threads.storageContent", { param: { id: "t1" } });

// @ts-expect-error the high-level seam also requires that query
void apiClient.threads[":id"]["thread-storage"].content.$get({
  param: { id: "t1" },
});

// Optional-query routes may still omit the query object.
void loomApiFetch("system.providers");

// A multipart route accepts the real browser body object.
void loomApiFetch("system.voiceTranscription", {
  formData: new FormData(),
});

// @ts-expect-error a multipart route requires its argument bag and body
void loomApiFetch("system.voiceTranscription");

// @ts-expect-error `filePath` is required by the catch-all path
void loomApiFetch("threads.worktreeFile", { param: { id: "t1" } });

// A method mismatch is a runtime refusal, not a compile one: the argument is a
// valid `LoomApiMethod`, and the route's own method is what rejects it. Covered
// by the runtime tests in `api-client.test.ts`.
void resolveLoomApiMethod("hosts.createJoinCode", "GET");
// @ts-expect-error an unknown route id is not accepted
void resolveLoomApiMethod("threads.notReal", "GET");

// --- W-583 thread runtime routes stay contract-bound ----------------------

declare const createThreadRequest: CreateThreadRequest;
declare const resolvePendingInteractionRequest: ResolvePendingInteractionRequest;
declare const sendMessageRequest: SendMessageRequest;
declare const updateThreadTabsRequest: UpdateThreadTabsRequest;
declare const updateUiPreferenceRequest: UpdateUiPreferenceRequest;

void loomApiFetch("threads.create", { json: createThreadRequest });
void loomApiFetch("threads.send", {
  param: { id: "t1" },
  json: sendMessageRequest,
});
void loomApiJson("threads.get", {
  param: { id: "t1" },
  query: { include: "environment,host" },
});
void loomApiJson("threads.timeline", {
  param: { id: "t1" },
  query: { afterSequence: "0", segmentLimit: "100" },
});
void loomApiJson("system.environmentProviders", {
  query: { projectId: "p1", hostId: "h1" },
});

void apiClient.threads[":id"].interactions.$get({
  param: { id: "t1" },
});
void apiClient.threads[":id"].interactions[":interactionId"].$get({
  param: { id: "t1", interactionId: "interaction-1" },
});
void apiClient.threads[":id"].interactions[":interactionId"].resolve.$post({
  param: { id: "t1", interactionId: "interaction-1" },
  json: resolvePendingInteractionRequest,
});
void apiClient.threads[":id"].interactions[":interactionId"].cancel.$post({
  param: { id: "t1", interactionId: "interaction-1" },
});
void apiClient.threads[":id"]["default-execution-options"].$get({
  param: { id: "t1" },
});
void apiClient.threads[":id"].read.$post({ param: { id: "t1" } });
void apiClient.threads[":id"].tabs.$get({ param: { id: "t1" } });
void apiClient.threads[":id"].tabs.$put({
  param: { id: "t1" },
  json: updateThreadTabsRequest,
});
void apiClient.threads[":id"].unread.$post({ param: { id: "t1" } });
void apiClient.preferences.ui.$get();
void apiClient.preferences.ui[":key"].$put({
  param: { key: "sidebar.collapsedProjects" },
  json: updateUiPreferenceRequest,
});
void apiClient.preferences.ui[":key"].$delete({
  param: { key: "sidebar.collapsedProjects" },
});

// @ts-expect-error a UI preference update requires its JSON body
void apiClient.preferences.ui[":key"].$put({
  param: { key: "sidebar.collapsedProjects" },
});

// @ts-expect-error a UI preference reset requires its path key
void apiClient.preferences.ui[":key"].$delete({});

// @ts-expect-error thread creation requires its contract JSON body
void loomApiFetch("threads.create");

// @ts-expect-error a thread send requires its path id
void loomApiFetch("threads.send", { json: sendMessageRequest });

void loomApiJson("threads.get", {
  param: { id: "t1" },
  // @ts-expect-error a thread read cannot carry a JSON body
  json: {},
});

// --- The route table is non-empty and unique -------------------------------

const routeIds = LOOM_API_ROUTES.map((route) => route.id);
void routeIds;

// --- `$url` and a request method are not the same shape --------------------

// A URL carries no body, so `$url()` on a body route needs no arguments...
void apiClient.system["voice-transcription"].$url();
void apiClient.system["voice-transcription"].$url({});
void apiClient.hosts["join-codes"].$url();

// ...while the request method must be given the body the contract declares.
// @ts-expect-error a JSON route cannot be called with no body
void apiClient.hosts["join-codes"].$post();

// @ts-expect-error a form route cannot be called with no body
void apiClient.system["voice-transcription"].$post();

void apiClient.hosts["join-codes"].$post({
  // @ts-expect-error multipart where the contract declares JSON
  formData: new FormData(),
});

// A query-only route still takes its URL arguments on both.
void apiClient.threads[":id"]["thread-storage"].content.$url({
  param: { id: "t1" },
  query: { path: "a.ts" },
});
// @ts-expect-error `$url` never takes a body
void apiClient.hosts["join-codes"].$url({ json: {} });
