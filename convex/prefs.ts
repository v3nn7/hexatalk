import { v } from "convex/values";
import { mutation, query, MutationCtx, QueryCtx } from "./_generated/server";
import { currentUser } from "./session";
import { Id } from "./_generated/dataModel";

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

  for (const m of members) {
    const user = await ctx.db.get("users", m.userId);
    if (user && user.storeChatHistory === false) {
      return false;
    }
    const pref = await ctx.db
      .query("chatStorePrefs")
      .withIndex("by_user_and_conversation", (q) =>
        q.eq("userId", m.userId).eq("conversationId", conversationId),
      )
      .unique();
    if (pref && pref.store === false) {
      return false;
    }
  }
  return true;
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
    const sessions = await ctx.db
      .query("sessions")
      .withIndex("by_userId", (q) => q.eq("userId", me._id))
      .take(100);
    let killed = 0;
    for (const s of sessions) {
      if (s.token !== args.sessionToken) {
        await ctx.db.delete("sessions", s._id);
        killed += 1;
      }
    }
    return { killed };
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
