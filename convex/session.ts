import { MutationCtx, QueryCtx } from "./_generated/server";
import { Doc, Id } from "./_generated/dataModel";

/** SHA-256 hex — duplicated from auth.ts's hashSessionToken to avoid a
 * circular import (auth.ts already imports from this file). */
async function hashSessionToken(token: string): Promise<string> {
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(token),
  );
  return Array.from(new Uint8Array(digest))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

/** Type guard: MutationCtx carries a scheduler, QueryCtx does not. */
export function isMutationCtx(ctx: QueryCtx | MutationCtx): ctx is MutationCtx {
  return "scheduler" in ctx;
}

export async function currentUser(
  ctx: QueryCtx | MutationCtx,
  sessionToken: string,
): Promise<Doc<"users">> {
  const tokenHash = await hashSessionToken(sessionToken);
  const hashed = await ctx.db
    .query("sessions")
    .withIndex("by_tokenHash", (q) => q.eq("tokenHash", tokenHash))
    .unique();
  let session = hashed;
  if (!session) {
    // Legacy rows written before token hashing still carry the plaintext
    // token; match it directly.
    const legacy = await ctx.db
      .query("sessions")
      .withIndex("by_token", (q) => q.eq("token", sessionToken))
      .unique();
    if (legacy) {
      session = legacy;
      // Lazy migration: replace the plaintext token with its hash so the
      // credential no longer sits in the DB in cleartext.
      if (isMutationCtx(ctx) && legacy.expiresAt >= Date.now()) {
        await ctx.db.patch("sessions", legacy._id, {
          tokenHash,
          token: undefined,
        });
      }
    }
  }
  if (!session || session.expiresAt < Date.now()) {
    // Hygiene: drop the expired row on the way out when we can write.
    if (session && isMutationCtx(ctx)) {
      await ctx.db.delete("sessions", session._id);
    }
    throw new Error("Session expired, please log in again");
  }
  const user = await ctx.db.get("users", session.userId);
  if (!user) {
    throw new Error("User not found");
  }
  if (user.banned) {
    throw new Error("This account has been banned by an administrator");
  }
  return user;
}

export type PlatformRole = "user" | "moderator" | "admin" | "owner";

/** Hard-pinned platform owners — rank cannot be stripped via the admin panel. */
export const OWNER_USERNAMES = ["veni"];

export function isPinnedOwnerUsername(username: string): boolean {
  return OWNER_USERNAMES.includes(username.trim().toLowerCase());
}

export function isPinnedOwner(user: Doc<"users">): boolean {
  return isPinnedOwnerUsername(user.username) || user.role === "owner";
}

export function platformRole(user: Doc<"users">): PlatformRole {
  if (isPinnedOwnerUsername(user.username) || user.role === "owner") {
    return "owner";
  }
  if (user.role === "admin") return "admin";
  if (user.role === "moderator") return "moderator";
  return "user";
}

/** Numeric rank for comparisons (higher = more power). */
export function platformRank(user: Doc<"users">): number {
  switch (platformRole(user)) {
    case "owner":
      return 200;
    case "admin":
      return 100;
    case "moderator":
      return 50;
    default:
      return 0;
  }
}

export function isStaff(user: Doc<"users">): boolean {
  return platformRank(user) >= 50;
}

/** Admin or owner — full platform control (custom slugs, promote staff, etc.). */
export async function requireAdmin(
  ctx: QueryCtx | MutationCtx,
  sessionToken: string,
): Promise<Doc<"users">> {
  const user = await currentUser(ctx, sessionToken);
  if (user.role !== "admin" && user.role !== "owner") {
    throw new Error("Admin permission required");
  }
  return user;
}

/** Admin or moderator (staff panel access). */
export async function requireStaff(
  ctx: QueryCtx | MutationCtx,
  sessionToken: string,
): Promise<Doc<"users">> {
  const user = await currentUser(ctx, sessionToken);
  if (!isStaff(user)) {
    throw new Error("Staff permission required");
  }
  return user;
}

/**
 * Staff (admin/moderator) cannot be blocked, kicked from servers they
 * don't own as casually, etc. Admins are fully protected; mods less so.
 */
export function isProtectedTarget(target: Doc<"users">): boolean {
  // Admins + platform owner: unkickable / unblockable.
  return platformRank(target) >= 100;
}

export async function isBlockedEitherWay(
  ctx: QueryCtx | MutationCtx,
  a: Id<"users">,
  b: Id<"users">,
): Promise<boolean> {
  const [userA, userB, aBlockedB, bBlockedA] = await Promise.all([
    ctx.db.get("users", a),
    ctx.db.get("users", b),
    ctx.db
      .query("blocks")
      .withIndex("by_blocker_and_blocked", (q) =>
        q.eq("blockerId", a).eq("blockedId", b),
      )
      .unique(),
    ctx.db
      .query("blocks")
      .withIndex("by_blocker_and_blocked", (q) =>
        q.eq("blockerId", b).eq("blockedId", a),
      )
      .unique(),
  ]);
  // Platform admins ignore blocks both ways for support access.
  if (userA && isProtectedTarget(userA)) return false;
  if (userB && isProtectedTarget(userB)) return false;
  return aBlockedB !== null || bBlockedA !== null;
}
