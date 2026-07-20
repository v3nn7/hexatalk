import { v } from "convex/values";
import { mutation, query, MutationCtx, QueryCtx } from "./_generated/server";
import { currentUser } from "./session";
import { hashSessionToken, SESSION_TTL_MS } from "./auth";
import { Id } from "./_generated/dataModel";

/** Hash-first token comparison with a plaintext fallback for legacy
 * sessions written before token hashing (same pattern as auth.signOut). */
function sessionMatchesToken(
  session: { token?: string; tokenHash?: string },
  sessionToken: string,
  tokenHash: string,
): boolean {
  if (session.tokenHash !== undefined) {
    return session.tokenHash === tokenHash;
  }
  return session.token === sessionToken;
}

/**
 * Returns false if any member has global storeChatHistory=false or a
 * per-conversation pref store=false. Used by messages:send.
 */
export async function conversationAllowsStorage(
  ctx: QueryCtx | MutationCtx,
  conversationId: Id<"conversations">,
): Promise<boolean> {
  const members = await ctx.db
    .query("conversationMembers")
    .withIndex("by_conversation", (q) => q.eq("conversationId", conversationId))
    .take(200);

  // Fan out member lookups in parallel instead of 2N sequential round-trips.
  const checks = await Promise.all(
    members.map(async (m) => {
      const [user, pref] = await Promise.all([
        ctx.db.get("users", m.userId),
        ctx.db
          .query("chatStorePrefs")
          .withIndex("by_user_and_conversation", (q) =>
            q.eq("userId", m.userId).eq("conversationId", conversationId),
          )
          .unique(),
      ]);
      if (user && user.storeChatHistory === false) return false;
      if (pref && pref.store === false) return false;
      return true;
    }),
  );
  return checks.every(Boolean);
}

export const setStoreChatHistory = mutation({
  args: {
    sessionToken: v.string(),
    store: v.boolean(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await ctx.db.patch("users", me._id, {
      storeChatHistory: args.store,
    });
    return null;
  },
});

export const setPrivacyFlags = mutation({
  args: {
    sessionToken: v.string(),
    hideOnlineStatus: v.optional(v.boolean()),
    friendsOnlyDms: v.optional(v.boolean()),
    discoverable: v.optional(v.boolean()),
    friendRequestPrivacy: v.optional(
      v.union(
        v.literal("everyone"),
        v.literal("mutual_servers"),
        v.literal("nobody"),
      ),
    ),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const patch: {
      hideOnlineStatus?: boolean;
      friendsOnlyDms?: boolean;
      discoverable?: boolean;
      friendRequestPrivacy?: "everyone" | "mutual_servers" | "nobody";
    } = {};
    if (args.hideOnlineStatus !== undefined) {
      patch.hideOnlineStatus = args.hideOnlineStatus;
    }
    if (args.friendsOnlyDms !== undefined) {
      patch.friendsOnlyDms = args.friendsOnlyDms;
    }
    if (args.discoverable !== undefined) {
      patch.discoverable = args.discoverable;
    }
    if (args.friendRequestPrivacy !== undefined) {
      patch.friendRequestPrivacy = args.friendRequestPrivacy;
    }
    await ctx.db.patch("users", me._id, patch);
    return null;
  },
});

/** Revoke every session except the current one. */
export const signOutOtherSessions = mutation({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const tokenHash = await hashSessionToken(args.sessionToken);
    const sessions = await ctx.db
      .query("sessions")
      .withIndex("by_userId", (q) => q.eq("userId", me._id))
      .take(100);
    let killed = 0;
    for (const s of sessions) {
      if (!sessionMatchesToken(s, args.sessionToken, tokenHash)) {
        await ctx.db.delete("sessions", s._id);
        killed += 1;
      }
    }
    return { killed };
  },
});

/** List active sessions for the security / devices UI. */
export const listSessions = query({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const tokenHash = await hashSessionToken(args.sessionToken);
    const sessions = await ctx.db
      .query("sessions")
      .withIndex("by_userId", (q) => q.eq("userId", me._id))
      .take(50);
    const now = Date.now();
    return sessions
      .filter((s) => s.expiresAt > now)
      .map((s) => ({
        sessionId: s._id,
        deviceName: s.deviceName ?? "Unknown device",
        platform: s.platform ?? "unknown",
        createdAt: s.createdAt ?? s._creationTime,
        lastActiveAt: s.lastActiveAt ?? s.createdAt ?? s._creationTime,
        isCurrent: sessionMatchesToken(s, args.sessionToken, tokenHash),
        expiresAt: s.expiresAt,
      }))
      .sort((a, b) => b.lastActiveAt - a.lastActiveAt);
  },
});

/** Revoke one session by id (cannot revoke others' sessions). */
export const revokeSession = mutation({
  args: {
    sessionToken: v.string(),
    sessionId: v.id("sessions"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const row = await ctx.db.get("sessions", args.sessionId);
    if (!row || row.userId !== me._id) {
      throw new Error("Session not found");
    }
    const tokenHash = await hashSessionToken(args.sessionToken);
    if (sessionMatchesToken(row, args.sessionToken, tokenHash)) {
      throw new Error("Use Log out to end this device's session");
    }
    await ctx.db.delete("sessions", args.sessionId);
    return null;
  },
});

/** Touch lastActiveAt + optional device label on the current session. */
export const touchSession = mutation({
  args: {
    sessionToken: v.string(),
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
    const tokenHash = await hashSessionToken(args.sessionToken);
    const session =
      (await ctx.db
        .query("sessions")
        .withIndex("by_tokenHash", (q) => q.eq("tokenHash", tokenHash))
        .unique()) ??
      // Legacy rows written before token hashing still carry the plaintext
      // token; match it directly.
      (await ctx.db
        .query("sessions")
        .withIndex("by_token", (q) => q.eq("token", args.sessionToken))
        .unique());
    if (!session || session.expiresAt < Date.now()) {
      throw new Error("Session expired, please log in again");
    }
    const now = Date.now();
    const patch: {
      lastActiveAt: number;
      expiresAt?: number;
      deviceName?: string;
      platform?:
        | "desktop"
        | "android"
        | "ios"
        | "web"
        | "bot"
        | "unknown";
    } = { lastActiveAt: now };
    // Sliding renewal: active sessions never die mid-use, idle ones still
    // expire on the original 30-day clock.
    if (session.expiresAt - now < SESSION_TTL_MS / 2) {
      patch.expiresAt = now + SESSION_TTL_MS;
    }
    if (args.deviceName !== undefined) {
      patch.deviceName = args.deviceName.trim().slice(0, 80) || "Unknown device";
    }
    if (args.platform !== undefined) {
      patch.platform = args.platform;
    }
    await ctx.db.patch("sessions", session._id, patch);
    return null;
  },
});

export const setConversationStore = mutation({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
    store: v.boolean(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const membership = await ctx.db
      .query("conversationMembers")
      .withIndex("by_conversation_and_user", (q) =>
        q.eq("conversationId", args.conversationId).eq("userId", me._id),
      )
      .unique();
    if (!membership) {
      throw new Error("You're not a member of this chat");
    }

    const existing = await ctx.db
      .query("chatStorePrefs")
      .withIndex("by_user_and_conversation", (q) =>
        q.eq("userId", me._id).eq("conversationId", args.conversationId),
      )
      .unique();
    if (existing) {
      await ctx.db.patch("chatStorePrefs", existing._id, {
        store: args.store,
      });
    } else {
      await ctx.db.insert("chatStorePrefs", {
        userId: me._id,
        conversationId: args.conversationId,
        store: args.store,
      });
    }
    return null;
  },
});

export const getConversationStore = query({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const membership = await ctx.db
      .query("conversationMembers")
      .withIndex("by_conversation_and_user", (q) =>
        q.eq("conversationId", args.conversationId).eq("userId", me._id),
      )
      .unique();
    if (!membership) {
      throw new Error("You're not a member of this chat");
    }
    const pref = await ctx.db
      .query("chatStorePrefs")
      .withIndex("by_user_and_conversation", (q) =>
        q.eq("userId", me._id).eq("conversationId", args.conversationId),
      )
      .unique();
    // Effective: per-chat pref overrides global default.
    const globalStore = me.storeChatHistory !== false;
    const store = pref ? pref.store : globalStore;
    const allows = await conversationAllowsStorage(ctx, args.conversationId);
    return {
      store,
      globalStore,
      conversationAllowsStorage: allows,
    };
  },
});
