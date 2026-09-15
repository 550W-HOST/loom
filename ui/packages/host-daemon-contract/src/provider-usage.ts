import { z } from "zod";

export const providerUsageWindowSchema = z
  .object({
    label: z.string().min(1),
    usedPercent: z.number().min(0).max(100),
    resetsAt: z.string().min(1).nullable(),
    cost: z
      .object({
        usedUsdCents: z.number().int().nonnegative(),
        limitUsdCents: z.number().int().positive(),
      })
      .optional(),
  })
  .passthrough();

export type ProviderUsageWindow = z.infer<typeof providerUsageWindowSchema>;

export const providerUsageSchema = z.discriminatedUnion("status", [
  z
    .object({
      status: z.literal("ok"),
      accountEmail: z.string().email().nullable(),
      planLabel: z.string().min(1).nullable(),
      windows: z.array(providerUsageWindowSchema),
    })
    .passthrough(),
  z.object({ status: z.literal("not_installed") }).passthrough(),
  z.object({ status: z.literal("unauthenticated") }).passthrough(),
  z.object({ status: z.literal("expired") }).passthrough(),
  z
    .object({
      status: z.literal("error"),
      message: z.string().min(1),
      planLabel: z.string().min(1).nullable().default(null),
      accountEmail: z.string().nullable().default(null),
    })
    .passthrough(),
]);

export type ProviderUsage = z.infer<typeof providerUsageSchema>;

export const providerUsageResultSchema = z.discriminatedUnion("supported", [
  z.object({ supported: z.literal(false) }).passthrough(),
  z
    .object({
      supported: z.literal(true),
      usage: providerUsageSchema,
    })
    .passthrough(),
]);

export type ProviderUsageResult = z.infer<typeof providerUsageResultSchema>;

export const providerUsageResponseSchema = z.record(
  z.string().min(1),
  providerUsageSchema,
);
export type ProviderUsageResponse = z.infer<typeof providerUsageResponseSchema>;
