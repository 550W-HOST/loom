// Minimal ambient types for the Bun APIs the exporter uses. The tool runs
// under bun; this keeps `tsc --noEmit` useful without pulling in all of
// `@types/bun`.

export {};

declare global {
  const Bun: {
    /** Resolve a bare specifier as if imported from `from`. */
    resolveSync(specifier: string, from: string): string;
    spawnSync(command: string[]): {
      exitCode: number;
      stdout: { toString(): string };
    };
  };

  interface ImportMeta {
    /** Directory of the current module (bun). */
    readonly dir: string;
  }
}
