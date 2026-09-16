import type { FilePreviewLineRange } from "@bb/client-core";

export type ExperimentalFileLocation =
  | { kind: "line"; line: number; column: number | null }
  | { kind: "range"; startLine: number; endLine: number };

export type ExperimentalLiveFileTarget =
  | { kind: "workspace"; environmentId: string; path: string }
  | { kind: "host"; hostId: string; path: string }
  | { kind: "thread-storage"; threadId: string; path: string };

export interface ExperimentalFileOpenOptions {
  target: ExperimentalLiveFileTarget;
  location: ExperimentalFileLocation | null;
}

const FILE_PATH_MAX_LENGTH = 32_768;
const WINDOWS_DRIVE_ABSOLUTE_PATH = /^[A-Za-z]:[\\/]/u;
const WINDOWS_UNC_ABSOLUTE_PATH = /^\\\\/u;

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return false;
  }
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}

function hasExactKeys(
  value: Record<string, unknown>,
  keys: readonly string[],
): boolean {
  const actualKeys = Object.keys(value);
  return (
    actualKeys.length === keys.length &&
    keys.every((key) => Object.prototype.hasOwnProperty.call(value, key))
  );
}

function isNonEmptyIdentity(value: unknown): value is string {
  return (
    typeof value === "string" &&
    value.length > 0 &&
    value.length <= FILE_PATH_MAX_LENGTH &&
    value.trim() === value
  );
}

function hasControlCharacter(value: string): boolean {
  for (const character of value) {
    const codePoint = character.codePointAt(0);
    if (codePoint !== undefined && codePoint < 0x20) return true;
  }
  return false;
}

function hasUnpairedSurrogate(value: string): boolean {
  for (let index = 0; index < value.length; index += 1) {
    const codeUnit = value.charCodeAt(index);
    if (codeUnit >= 0xd800 && codeUnit <= 0xdbff) {
      if (index + 1 >= value.length) return true;
      const nextCodeUnit = value.charCodeAt(index + 1);
      if (nextCodeUnit < 0xdc00 || nextCodeUnit > 0xdfff) return true;
      index += 1;
    } else if (codeUnit >= 0xdc00 && codeUnit <= 0xdfff) {
      return true;
    }
  }
  return false;
}

function isValidPathSegment(segment: string): boolean {
  return segment.length > 0 && segment !== "." && segment !== "..";
}

function isPositiveSafeInteger(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value > 0;
}

function isValidRelativeFilePath(value: unknown): value is string {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    value.length > FILE_PATH_MAX_LENGTH ||
    value.trim() !== value ||
    value.includes("\\") ||
    hasControlCharacter(value) ||
    hasUnpairedSurrogate(value)
  ) {
    return false;
  }
  return value.split("/").every(isValidPathSegment);
}

function isValidAbsoluteHostFilePath(value: unknown): value is string {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    value.length > FILE_PATH_MAX_LENGTH ||
    value.trim() !== value ||
    hasControlCharacter(value) ||
    hasUnpairedSurrogate(value)
  ) {
    return false;
  }

  if (value.startsWith("/") && !value.startsWith("//")) {
    const segments = value.slice(1).split("/");
    return segments.length > 0 && segments.every(isValidPathSegment);
  }
  if (WINDOWS_DRIVE_ABSOLUTE_PATH.test(value)) {
    const segments = value.slice(3).split(/[\\/]/u);
    return segments.length > 0 && segments.every(isValidPathSegment);
  }
  if (WINDOWS_UNC_ABSOLUTE_PATH.test(value)) {
    const segments = value.slice(2).split(/[\\/]/u);
    return segments.length >= 3 && segments.every(isValidPathSegment);
  }
  return false;
}

export function normalizeExperimentalLiveFileTarget(
  value: unknown,
): ExperimentalLiveFileTarget | null {
  if (!isRecord(value)) return null;
  switch (value.kind) {
    case "workspace":
      return hasExactKeys(value, ["kind", "environmentId", "path"]) &&
        isNonEmptyIdentity(value.environmentId) &&
        isValidRelativeFilePath(value.path)
        ? {
            kind: "workspace",
            environmentId: value.environmentId,
            path: value.path,
          }
        : null;
    case "host":
      return hasExactKeys(value, ["kind", "hostId", "path"]) &&
        isNonEmptyIdentity(value.hostId) &&
        isValidAbsoluteHostFilePath(value.path)
        ? { kind: "host", hostId: value.hostId, path: value.path }
        : null;
    case "thread-storage":
      return hasExactKeys(value, ["kind", "threadId", "path"]) &&
        isNonEmptyIdentity(value.threadId) &&
        isValidRelativeFilePath(value.path)
        ? {
            kind: "thread-storage",
            threadId: value.threadId,
            path: value.path,
          }
        : null;
    default:
      return null;
  }
}

function normalizeExperimentalFileLocation(
  value: unknown,
): ExperimentalFileLocation | null | undefined {
  if (value === null) return null;
  if (!isRecord(value)) return undefined;
  if (value.kind === "line") {
    return hasExactKeys(value, ["kind", "line", "column"]) &&
      isPositiveSafeInteger(value.line) &&
      (value.column === null || isPositiveSafeInteger(value.column))
      ? { kind: "line", line: value.line, column: value.column }
      : undefined;
  }
  if (value.kind === "range") {
    return hasExactKeys(value, ["kind", "startLine", "endLine"]) &&
      isPositiveSafeInteger(value.startLine) &&
      isPositiveSafeInteger(value.endLine) &&
      value.startLine <= value.endLine
      ? {
          kind: "range",
          startLine: value.startLine,
          endLine: value.endLine,
        }
      : undefined;
  }
  return undefined;
}

export function normalizeExperimentalFileOpenOptions(
  value: unknown,
): ExperimentalFileOpenOptions | null {
  if (!isRecord(value)) return null;
  const target = normalizeExperimentalLiveFileTarget(value.target);
  const location = normalizeExperimentalFileLocation(value.location);
  return target === null || location === undefined
    ? null
    : { target, location };
}

export function getExperimentalFileLocationStart(
  location: ExperimentalFileLocation | null,
): { columnNumber: number | null; lineNumber: number | null } {
  if (location === null) return { columnNumber: null, lineNumber: null };
  if (location.kind === "line") {
    return { columnNumber: location.column, lineNumber: location.line };
  }
  return { columnNumber: null, lineNumber: location.startLine };
}

export function toFilePreviewLineRange(
  location: ExperimentalFileLocation | null,
): FilePreviewLineRange | null {
  if (location === null) return null;
  return {
    startLineNumber:
      location.kind === "line" ? location.line : location.startLine,
    endLineNumber: location.kind === "line" ? location.line : location.endLine,
  };
}

export function getFileBasename(path: string): string {
  const normalizedPath = path.replace(/[\\/]+$/u, "");
  return normalizedPath.split(/[\\/]/u).at(-1) ?? path;
}
