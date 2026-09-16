import { z } from "zod";
import {
  FILE_LIST_EXCLUDE_NAME_MAX_LENGTH,
  FILE_LIST_EXCLUDE_NAMES_MAX,
  FILE_LIST_LIMIT_MAX,
} from "@bb/domain";

interface HostFileReadResult {
  path: string;
  content: string;
  contentEncoding: "base64" | "utf8";
  mimeType?: string;
  sizeBytes: number;
  modifiedAtMs?: number;
  sha256: string;
}

export const hostFileReadRequestSchema = z
  .object({
    hostId: z.string().min(1).optional(),
    path: z.string().min(1),
    rootPath: z.string().min(1).optional(),
  })
  .strict();
export type HostFileReadRequest = z.infer<typeof hostFileReadRequestSchema>;

export const hostFileWriteRequestSchema = z
  .object({
    hostId: z.string().min(1).optional(),
    path: z.string().min(1),
    rootPath: z.string().min(1).optional(),
    content: z.string(),
    contentEncoding: z.enum(["utf8", "base64"]).optional(),
    createParents: z.boolean().optional(),
    expectedSha256: z.string().nullable().optional(),
    mode: z.number().int().min(0).max(0o777).optional(),
  })
  .strict();
export type HostFileWriteRequest = z.infer<typeof hostFileWriteRequestSchema>;

const fileListExcludeNamesRequestSchema = z
  .array(z.string().min(1).max(FILE_LIST_EXCLUDE_NAME_MAX_LENGTH))
  .max(FILE_LIST_EXCLUDE_NAMES_MAX)
  .optional();

export const hostFileListRequestSchema = z
  .object({
    hostId: z.string().min(1).optional(),
    path: z.string().min(1),
    query: z.string().optional(),
    limit: z.number().int().positive().max(FILE_LIST_LIMIT_MAX).optional(),
    includeHidden: z.boolean().optional(),
    excludeNames: fileListExcludeNamesRequestSchema,
  })
  .strict();
export type HostFileListRequest = z.infer<typeof hostFileListRequestSchema>;

export const hostPathListRequestSchema = z
  .object({
    hostId: z.string().min(1).optional(),
    path: z.string().min(1),
    query: z.string().optional(),
    limit: z.number().int().positive().max(FILE_LIST_LIMIT_MAX).optional(),
    includeFiles: z.boolean(),
    includeDirectories: z.boolean(),
    includeHidden: z.boolean().optional(),
    excludeNames: fileListExcludeNamesRequestSchema,
  })
  .strict();
export type HostPathListRequest = z.infer<typeof hostPathListRequestSchema>;

export const hostMkdirRequestSchema = z
  .object({
    hostId: z.string().min(1).optional(),
    path: z.string().min(1),
    rootPath: z.string().min(1).optional(),
    recursive: z.boolean().optional(),
  })
  .strict();
export type HostMkdirRequest = z.infer<typeof hostMkdirRequestSchema>;

export const hostMovePathRequestSchema = z
  .object({
    hostId: z.string().min(1).optional(),
    sourcePath: z.string().min(1),
    destinationPath: z.string().min(1),
    rootPath: z.string().min(1).optional(),
  })
  .strict();
export type HostMovePathRequest = z.infer<typeof hostMovePathRequestSchema>;

export const hostRemovePathRequestSchema = z
  .object({
    hostId: z.string().min(1).optional(),
    path: z.string().min(1),
    rootPath: z.string().min(1).optional(),
    recursive: z.boolean().optional(),
  })
  .strict();
export type HostRemovePathRequest = z.infer<typeof hostRemovePathRequestSchema>;

export const createFilePreviewRequestSchema = z
  .object({
    hostId: z.string().min(1).optional(),
    rootPath: z.string().min(1),
    ttlMs: z.number().int().min(60_000).max(3_600_000).optional(),
  })
  .strict();
export type CreateFilePreviewRequest = z.infer<
  typeof createFilePreviewRequestSchema
>;
export interface CreateFilePreviewResponse {
  baseUrl: string;
  expiresAtMs: number;
}

export type HostFileReadResponse = HostFileReadResult;
export type HostFileWriteResponse =
  | { outcome: "written"; sha256: string; sizeBytes: number }
  | { outcome: "conflict"; currentSha256: string | null };
export interface HostFileListResponse {
  files: Array<{ path: string; name: string }>;
  truncated: boolean;
}
export interface HostPathListResponse {
  paths: Array<{ path: string; name: string; type: "file" | "directory" }>;
  truncated: boolean;
}
export type HostMkdirResponse = { ok: true };
export type HostMovePathResponse = { ok: true };
export type HostRemovePathResponse = { ok: true };
