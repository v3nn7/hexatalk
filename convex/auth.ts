import { v } from "convex/values";
import {
  action,
  internalMutation,
  internalQuery,
  mutation,
  query,
} from "./_generated/server";
import { api, internal } from "./_generated/api";
import { Doc, Id } from "./_generated/dataModel";
import {
  currentUser,
  OWNER_USERNAMES,
  PlatformRole,
  platformRole as resolvePlatformRole,
} from "./session";

const PBKDF2_ITERATIONS = 100_000;
const SESSION_TTL_MS = 30 * 24 * 60 * 60 * 1000;
/** Extra usernames that always get "admin" (not owner). */
const ADMIN_USERNAMES: string[] = [];
const MAX_FAILED_LOGIN_ATTEMPTS = 5;
const LOGIN_LOCKOUT_MS = 5 * 60 * 1000;

type AuthResult = {
  token: string;
  userId: Id<"users">;
  username: string;
  displayName: string;
  role: PlatformRole;
  email: string;
  emailVerified: boolean;
};

function forcedRoleForUsername(username: string): PlatformRole | null {
  if (OWNER_USERNAMES.includes(username)) return "owner";
  if (ADMIN_USERNAMES.includes(username)) return "admin";
  return null;
}

export function bytesToHex(bytes: Uint8Array): string {
  return Array.from(bytes)
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

export function randomHex(byteLength: number): string {
  const bytes = new Uint8Array(byteLength);
  crypto.getRandomValues(bytes);
  return bytesToHex(bytes);
}

function hexToBytes(hex: string): Uint8Array {
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < bytes.length; i++) {
    bytes[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  }
  return bytes;
}

/** SHA-256 hex of a session token; only this hash is stored in the DB. */
export async function hashSessionToken(token: string): Promise<string> {
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(token),
  );
  return bytesToHex(new Uint8Array(digest));
}

export async function hashPassword(password: string, saltHex: string): Promise<string> {
  const salt = hexToBytes(saltHex);
  const keyMaterial = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(password),
    "PBKDF2",
    false,
    ["deriveBits"],
  );
  const bits = await crypto.subtle.deriveBits(
    { name: "PBKDF2", salt: salt as BufferSource, iterations: PBKDF2_ITERATIONS, hash: "SHA-256" },
    keyMaterial,
    256,
  );
  return bytesToHex(new Uint8Array(bits));
}

export function timingSafeEqual(a: string, b: string): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) {
    diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  }
  return diff === 0;
}

export const signUp = action({
  args: {
    username: v.string(),
    password: v.string(),
    displayName: v.string(),
    email: v.string(),
  },
  handler: async (ctx, args): Promise<AuthResult> => {
    const username = args.username.trim().toLowerCase();
    const displayName = args.displayName.trim() || args.username.trim();
    const email = args.email.trim().toLowerCase();

    if (username.length < 3) {
      throw new Error("Username must be at least 3 characters");
    }
    if (username.length > 32) {
      throw new Error("Username must be 32 characters or fewer");
    }
    if (displayName.length > 50) {
      throw new Error("Display name is too long");
    }
    if (args.password.length < 6) {
      throw new Error("Password must be at least 6 characters");
    }
    if (args.password.length > 128) {
      throw new Error("Password is too long");
    }
    if (email.length > 254 || !/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email)) {
      throw new Error("Enter a valid email address");
    }

    const existing: Doc<"users"> | null = await ctx.runQuery(
      internal.auth.getUserByUsername,
      { username },
    );
    if (existing) {
      throw new Error("This username is taken");
    }
    const existingEmail = await ctx.runQuery(internal.email.getUserByEmail, { email });
    if (existingEmail) {
      throw new Error("This email is already in use");
    }

    const salt = randomHex(16);
    const passwordHash = await hashPassword(args.password, salt);
    const role: PlatformRole = forcedRoleForUsername(username) ?? "user";

    const userId: Id<"users"> = await ctx.runMutation(internal.auth.createUser, {
      username,
      displayName,
      salt,
      passwordHash,
      role,
    });

    const token = randomHex(32);
    await ctx.runMutation(internal.auth.createSession, {
      userId,
      tokenHash: await hashSessionToken(token),
      expiresAt: Date.now() + SESSION_TTL_MS,
    });

    // Fire off the verification code; the client gates the new account on
    // a "verify your email" screen until this comes back confirmed.
    await ctx.runAction(internal.email.issueCodeForUser, { userId, email });

    return { token, userId, username, displayName, role, email, emailVerified: false };
  },
});

export const signIn = action({
  args: {
    username: v.string(),
    password: v.string(),
  },
  handler: async (ctx, args): Promise<AuthResult> => {
    const username = args.username.trim().toLowerCase();

    const lockout: { lockedUntil: number | null } = await ctx.runQuery(
      internal.auth.getLoginLockout,
      { username },
    );
    if (lockout.lockedUntil && lockout.lockedUntil > Date.now()) {
      const secondsLeft = Math.ceil((lockout.lockedUntil - Date.now()) / 1000);
      throw new Error(
        `Too many failed attempts. Try again in ${secondsLeft}s.`,
      );
    }

    const user: Doc<"users"> | null = await ctx.runQuery(
      internal.auth.getUserByUsername,
      { username },
    );
    if (!user || !user.passwordHash || !user.salt) {
      await ctx.runMutation(internal.auth.recordFailedLogin, { username });
      throw new Error("Invalid username or password");
    }

    const attemptHash = await hashPassword(args.password, user.salt);
    if (!timingSafeEqual(attemptHash, user.passwordHash)) {
      await ctx.runMutation(internal.auth.recordFailedLogin, { username });
      throw new Error("Invalid username or password");
    }
    await ctx.runMutation(internal.auth.clearLoginAttempts, { username });

    if (user.banned) {
      throw new Error("This account has been banned by an administrator");
    }

    let role: PlatformRole =
      (user.role as PlatformRole | undefined) ?? "user";
    const forced = forcedRoleForUsername(user.username);
    if (forced && role !== forced) {
      role = forced;
      await ctx.runMutation(internal.auth.setRole, {
        userId: user._id,
        role,
      });
    }

    const token = randomHex(32);
    await ctx.runMutation(internal.auth.createSession, {
      userId: user._id,
      tokenHash: await hashSessionToken(token),
      expiresAt: Date.now() + SESSION_TTL_MS,
    });

    return {
      token,
      userId: user._id,
      username: user.username,
      displayName: user.displayName,
      role,
      email: user.email ?? "",
      emailVerified: user.emailVerified === true,
    };
  },
});

export const getLoginLockout = internalQuery({
  args: { username: v.string() },
  handler: async (ctx, args) => {
    const row = await ctx.db
      .query("loginAttempts")
      .withIndex("by_username", (q) => q.eq("username", args.username))
      .unique();
    return { lockedUntil: row?.lockedUntil ?? null };
  },
});

export const recordFailedLogin = internalMutation({
  args: { username: v.string() },
  handler: async (ctx, args) => {
    const row = await ctx.db
      .query("loginAttempts")
      .withIndex("by_username", (q) => q.eq("username", args.username))
      .unique();
    const failedCount = (row?.failedCount ?? 0) + 1;
    const lockedUntil =
      failedCount >= MAX_FAILED_LOGIN_ATTEMPTS
        ? Date.now() + LOGIN_LOCKOUT_MS
        : undefined;
    if (row) {
      await ctx.db.patch("loginAttempts", row._id, { failedCount, lockedUntil });
    } else {
      await ctx.db.insert("loginAttempts", {
        username: args.username,
        failedCount,
        lockedUntil,
      });
    }
  },
});

export const clearLoginAttempts = internalMutation({
  args: { username: v.string() },
  handler: async (ctx, args) => {
    const row = await ctx.db
      .query("loginAttempts")
      .withIndex("by_username", (q) => q.eq("username", args.username))
      .unique();
    if (row) {
      await ctx.db.delete("loginAttempts", row._id);
    }
  },
});

export const createUser = internalMutation({
  args: {
    username: v.string(),
    displayName: v.string(),
    salt: v.string(),
    passwordHash: v.string(),
    role: v.union(
      v.literal("user"),
      v.literal("moderator"),
      v.literal("admin"),
      v.literal("owner"),
    ),
  },
  handler: async (ctx, args) => {
    return await ctx.db.insert("users", args);
  },
});

export const setRole = internalMutation({
  args: {
    userId: v.id("users"),
    role: v.union(
      v.literal("user"),
      v.literal("moderator"),
      v.literal("admin"),
      v.literal("owner"),
    ),
  },
  handler: async (ctx, args) => {
    const user = await ctx.db.get("users", args.userId);
    // Hard lock: pinned owners always stay owner (cannot be demoted).
    if (user && OWNER_USERNAMES.includes(user.username)) {
      await ctx.db.patch("users", args.userId, { role: "owner" });
      return;
    }
    await ctx.db.patch("users", args.userId, { role: args.role });
  },
});

export const createSession = internalMutation({
  args: {
    userId: v.id("users"),
    tokenHash: v.string(),
    expiresAt: v.number(),
    deviceName: v.optional(v.string()),
    platform: v.optional(
      v.union(
        v.literal("desktop"),
        v.literal("android"),
        v.literal("ios"),
        v.literal("web"),
        v.literal("bot"),
        v.literal("unknown"),
      ),
    ),
  },
  handler: async (ctx, args) => {
    const now = Date.now();
    await ctx.db.insert("sessions", {
      userId: args.userId,
      tokenHash: args.tokenHash,
      expiresAt: args.expiresAt,
      deviceName: args.deviceName ?? "Unknown device",
      platform: args.platform ?? "unknown",
      createdAt: now,
      lastActiveAt: now,
    });
  },
});

export const setPasswordHash = internalMutation({
  args: {
    userId: v.id("users"),
    salt: v.string(),
    passwordHash: v.string(),
  },
  handler: async (ctx, args) => {
    await ctx.db.patch("users", args.userId, {
      salt: args.salt,
      passwordHash: args.passwordHash,
    });
  },
});

export const changePassword = action({
  args: {
    sessionToken: v.string(),
    currentPassword: v.string(),
    newPassword: v.string(),
  },
  handler: async (ctx, args) => {
    const user: Doc<"users"> = await ctx.runQuery(internal.auth.resolveSessionUser, {
      sessionToken: args.sessionToken,
    });

    if (!user.passwordHash || !user.salt) {
      throw new Error("This account signs in via Clerk and has no local password");
    }
    const attemptHash = await hashPassword(args.currentPassword, user.salt);
    if (!timingSafeEqual(attemptHash, user.passwordHash)) {
      throw new Error("Current password is incorrect");
    }
    if (args.newPassword.length < 6) {
      throw new Error("Password must be at least 6 characters");
    }
    if (args.newPassword.length > 128) {
      throw new Error("Password is too long");
    }

    const salt = randomHex(16);
    const passwordHash = await hashPassword(args.newPassword, salt);
    await ctx.runMutation(internal.auth.setPasswordHash, {
      userId: user._id,
      salt,
      passwordHash,
    });
    // Password change revokes every other session of this user.
    await ctx.runMutation(api.prefs.signOutOtherSessions, {
      sessionToken: args.sessionToken,
    });
    return null;
  },
});

export const signOut = mutation({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const tokenHash = await hashSessionToken(args.sessionToken);
    const session =
      (await ctx.db
        .query("sessions")
        .withIndex("by_tokenHash", (q) => q.eq("tokenHash", tokenHash))
        .unique()) ??
      // Legacy rows written before token hashing still carry the plaintext
      // token; match it directly (the row is deleted right after anyway).
      (await ctx.db
        .query("sessions")
        .withIndex("by_token", (q) => q.eq("token", args.sessionToken))
        .unique());
    if (session) {
      await ctx.db.delete("sessions", session._id);
    }
    return null;
  },
});

export const me = query({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const user = await currentUser(ctx, args.sessionToken);
    const avatarImageUrl = user.avatarStorageId
      ? await ctx.storage.getUrl(user.avatarStorageId)
      : null;
    return {
      userId: user._id,
      username: user.username,
      displayName: user.displayName,
      role: resolvePlatformRole(user),
      avatarColor: user.avatarColor ?? "",
      statusMessage: user.statusMessage ?? "",
      bio: user.bio ?? "",
      avatarImageUrl: avatarImageUrl ?? "",
      storeChatHistory: user.storeChatHistory !== false,
      hideOnlineStatus: user.hideOnlineStatus === true,
      friendsOnlyDms: user.friendsOnlyDms === true,
      discoverable: user.discoverable !== false,
      friendRequestPrivacy: user.friendRequestPrivacy ?? "everyone",
      presenceStatus: user.presenceStatus ?? "online",
      email: user.email ?? "",
      emailVerified: user.emailVerified === true,
    };
  },
});

export const resolveSessionUser = internalQuery({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    return await currentUser(ctx, args.sessionToken);
  },
});

export const getUserByUsername = internalQuery({
  args: { username: v.string() },
  handler: async (ctx, args) => {
    return await ctx.db
      .query("users")
      .withIndex("by_username", (q) => q.eq("username", args.username))
      .unique();
  },
});
