import { ConvexError, v } from "convex/values";
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

/**
 * User-facing failures for clients that call Convex over the public HTTP
 * API (mobile). Plain `throw new Error(...)` is redacted to a bare
 * "Server Error" on that path; `ConvexError` keeps the message.
 */
function authError(message: string): never {
  throw new ConvexError(message);
}

const PBKDF2_ITERATIONS = 100_000;
export const SESSION_TTL_MS = 30 * 24 * 60 * 60 * 1000;
/** Extra usernames that always get "admin" (not owner). */
const ADMIN_USERNAMES: string[] = [];
const MAX_FAILED_LOGIN_ATTEMPTS = 5;
const LOGIN_LOCKOUT_MS = 5 * 60 * 1000;
/** Discord-like username charset (checked after trim+lowercase). */
const USERNAME_PATTERN = /^[a-z0-9_.]+$/;
/** Fixed salt for the dummy hash used to equalize sign-in timing. */
export const DUMMY_SALT_HEX = "00000000000000000000000000000000";

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
      authError("Username must be at least 3 characters");
    }
    if (username.length > 32) {
      authError("Username must be 32 characters or fewer");
    }
    if (!USERNAME_PATTERN.test(username)) {
      authError(
        "Username can only contain lowercase letters, numbers, underscores and dots",
      );
    }
    if (username.startsWith(".") || username.endsWith(".")) {
      authError("Username can't start or end with a dot");
    }
    if (displayName.length > 50) {
      authError("Display name is too long");
    }
    if (args.password.length < 6) {
      authError("Password must be at least 6 characters");
    }
    if (args.password.length > 128) {
      authError("Password is too long");
    }
    if (email.length > 254 || !/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email)) {
      authError("Enter a valid email address");
    }

    const existing: Doc<"users"> | null = await ctx.runQuery(
      internal.auth.getUserByUsername,
      { username },
    );
    if (existing) {
      authError("This username is taken");
    }
    const existingEmail = await ctx.runQuery(internal.email.getUserByEmail, { email });
    if (existingEmail) {
      authError("This email is already in use");
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

    const user: Doc<"users"> | null = await ctx.runQuery(
      internal.auth.getUserByUsername,
      { username },
    );
    if (!user || !user.passwordHash || !user.salt) {
      // Timing equalization: run the same PBKDF2 work as a real password
      // check so "user doesn't exist" isn't distinguishable by latency.
      // No failed attempt is recorded and no lockout is checked for
      // nonexistent accounts — otherwise anyone could pre-lock a victim's
      // future username, and the lockout error would oracle which
      // usernames exist.
      await hashPassword(args.password, DUMMY_SALT_HEX);
      authError("Invalid username or password");
    }

    const lockout: { lockedUntil: number | null } = await ctx.runQuery(
      internal.auth.getLoginLockout,
      { username },
    );
    if (lockout.lockedUntil && lockout.lockedUntil > Date.now()) {
      const secondsLeft = Math.ceil((lockout.lockedUntil - Date.now()) / 1000);
      authError(`Too many failed attempts. Try again in ${secondsLeft}s.`);
    }
    if (lockout.lockedUntil) {
      // Lockout expired — reset the counter so the user gets a fresh set
      // of attempts instead of an instant re-lock on the next typo.
      await ctx.runMutation(internal.auth.clearLoginAttempts, { username });
    }

    const attemptHash = await hashPassword(args.password, user.salt);
    if (!timingSafeEqual(attemptHash, user.passwordHash)) {
      await ctx.runMutation(internal.auth.recordFailedLogin, { username });
      authError("Invalid username or password");
    }
    await ctx.runMutation(internal.auth.clearLoginAttempts, { username });

    if (user.banned) {
      authError("This account has been banned by an administrator");
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
      authError("This account signs in via Clerk and has no local password");
    }
    const attemptHash = await hashPassword(args.currentPassword, user.salt);
    if (!timingSafeEqual(attemptHash, user.passwordHash)) {
      authError("Current password is incorrect");
    }
    if (args.newPassword.length < 6) {
      authError("Password must be at least 6 characters");
    }
    if (args.newPassword.length > 128) {
      authError("Password is too long");
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

/**
 * Logged-out password reset — step 1: email a 6-digit code.
 *
 * Always returns the same shape so we don't leak whether an email is
 * registered. Real sends only happen for verified local-password accounts.
 */
export const requestPasswordReset = action({
  args: { email: v.string() },
  handler: async (ctx, args): Promise<{ ok: true }> => {
    const email = args.email.trim().toLowerCase();
    if (email.length > 254 || !/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email)) {
      authError("Enter a valid email address");
    }

    // Timing equalization when no matching user (same dummy PBKDF2 cost
    // isn't needed here — we always do the same "look up + maybe send"
    // path and never say "not found").
    const user = await ctx.runQuery(internal.email.getUserByEmail, { email });
    if (
      user &&
      !user.isBot &&
      user.emailVerified === true &&
      user.passwordHash &&
      user.salt &&
      !user.banned
    ) {
      const codeState = await ctx.runQuery(internal.email.getPasswordResetCodeState, {
        userId: user._id,
      });
      const sinceLast = Date.now() - codeState.lastSentAt;
      // Reuse the same 60s resend floor as email verification.
      if (codeState.lastSentAt > 0 && sinceLast < 60_000) {
        const waitSeconds = Math.ceil((60_000 - sinceLast) / 1000);
        authError(`Please wait ${waitSeconds}s before requesting another code`);
      }
      try {
        await ctx.runAction(internal.email.issuePasswordResetCode, {
          userId: user._id,
          email,
        });
      } catch (err) {
        // Surface provider misconfig; otherwise swallow so we don't leak.
        const msg = err instanceof Error ? err.message : String(err);
        if (msg.includes("RESEND_API_KEY") || msg.includes("not configured")) {
          authError("Email sending is not configured on the server");
        }
        authError("Could not send reset email — try again later");
      }
    }

    return { ok: true };
  },
});

/**
 * Logged-out password reset — step 2: code + new password.
 * On success all sessions for the user are revoked; client must sign in again.
 */
export const resetPasswordWithCode = action({
  args: {
    email: v.string(),
    code: v.string(),
    newPassword: v.string(),
  },
  handler: async (ctx, args): Promise<{ ok: true }> => {
    const email = args.email.trim().toLowerCase();
    const code = args.code.trim();
    if (email.length > 254 || !/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email)) {
      authError("Enter a valid email address");
    }
    if (!/^\d{6}$/.test(code)) {
      authError("Enter the 6-digit code from your email");
    }
    if (args.newPassword.length < 6) {
      authError("Password must be at least 6 characters");
    }
    if (args.newPassword.length > 128) {
      authError("Password is too long");
    }

    const user = await ctx.runQuery(internal.email.getUserByEmail, { email });
    if (!user || user.isBot || !user.passwordHash) {
      // Same message as a bad code so existence isn't leaked by this step.
      authError("Invalid or expired code");
    }

    const row = await ctx.runQuery(internal.email.getPasswordResetCodeByEmail, {
      email,
    });
    if (!row || row.userId !== user._id) {
      authError("Invalid or expired code");
    }
    if (row.expiresAt < Date.now()) {
      await ctx.runMutation(internal.email.deletePasswordResetCode, {
        codeId: row._id,
      });
      authError("Code expired — request a new one");
    }
    if (row.attempts >= 5) {
      await ctx.runMutation(internal.email.deletePasswordResetCode, {
        codeId: row._id,
      });
      authError("Too many attempts — request a new code");
    }

    const attemptHash = await hashSessionToken(code);
    if (!timingSafeEqual(attemptHash, row.codeHash)) {
      await ctx.runMutation(internal.email.bumpPasswordResetAttempts, {
        codeId: row._id,
        attempts: row.attempts + 1,
      });
      authError("Incorrect code");
    }

    const salt = randomHex(16);
    const passwordHash = await hashPassword(args.newPassword, salt);
    await ctx.runMutation(internal.auth.setPasswordHash, {
      userId: user._id,
      salt,
      passwordHash,
    });
    await ctx.runMutation(internal.email.deletePasswordResetCode, {
      codeId: row._id,
    });
    await ctx.runMutation(internal.auth.clearLoginAttempts, {
      username: user.username,
    });
    // Force re-login everywhere after a reset.
    await ctx.runMutation(internal.auth.revokeAllSessionsForUser, {
      userId: user._id,
    });

    return { ok: true };
  },
});

export const revokeAllSessionsForUser = internalMutation({
  args: { userId: v.id("users") },
  handler: async (ctx, args) => {
    const sessions = await ctx.db
      .query("sessions")
      .withIndex("by_userId", (q) => q.eq("userId", args.userId))
      .take(200);
    for (const s of sessions) {
      await ctx.db.delete("sessions", s._id);
    }
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
    const plusExpiresAt = user.plusExpiresAt ?? 0;
    const plusActive =
      typeof user.plusExpiresAt === "number" && user.plusExpiresAt > Date.now();
    const profileBannerUrl =
      plusActive && user.profileBannerStorageId
        ? ((await ctx.storage.getUrl(user.profileBannerStorageId)) ?? "")
        : "";
    return {
      userId: user._id,
      username: user.username,
      displayName: user.displayName,
      role: resolvePlatformRole(user),
      avatarColor: user.avatarColor ?? "",
      statusMessage: user.statusMessage ?? "",
      bio: user.bio ?? "",
      avatarImageUrl: avatarImageUrl ?? "",
      profileBannerUrl,
      storeChatHistory: user.storeChatHistory !== false,
      hideOnlineStatus: user.hideOnlineStatus === true,
      friendsOnlyDms: user.friendsOnlyDms === true,
      discoverable: user.discoverable !== false,
      friendRequestPrivacy: user.friendRequestPrivacy ?? "everyone",
      presenceStatus: user.presenceStatus ?? "online",
      email: user.email ?? "",
      emailVerified: user.emailVerified === true,
      plusActive,
      plusExpiresAt: plusActive ? plusExpiresAt : 0,
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

/**
 * Maintenance: delete expired sessions and expired e-mail verification
 * codes. Wire into crons.ts (daily) — see note for the integrator.
 * Bounded per run; safe to schedule repeatedly.
 */
export const cleanupExpiredAuthArtifacts = internalMutation({
  args: {},
  handler: async (ctx) => {
    const now = Date.now();

    const sessions = await ctx.db.query("sessions").take(1000);
    let sessionsDeleted = 0;
    for (const s of sessions) {
      if (s.expiresAt < now) {
        await ctx.db.delete("sessions", s._id);
        sessionsDeleted += 1;
      }
    }

    const codes = await ctx.db.query("emailVerificationCodes").take(500);
    let codesDeleted = 0;
    for (const c of codes) {
      if (c.expiresAt < now) {
        await ctx.db.delete("emailVerificationCodes", c._id);
        codesDeleted += 1;
      }
    }

    return { sessionsDeleted, codesDeleted };
  },
});
