import { v } from "convex/values";
import { mutation, query, internalQuery } from "./_generated/server";
import { currentUser } from "./session";
import { Id } from "./_generated/dataModel";
import { isEffectivelyMuted } from "./channels";

/**
 * Register / refresh a device push token for the signed-in user.
 * Actual FCM/APNs send is wired later (needs server keys in Convex env):
 * set FCM_SERVER_KEY / APNS_* in Convex dashboard, then call sendToUser.
 */
export const registerToken = mutation({
  args: {
    sessionToken: v.string(),
    token: v.string(),
    platform: v.union(
      v.literal("android"),
      v.literal("ios"),
      v.literal("desktop"),
      v.literal("web"),
    ),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const token = args.token.trim();
    if (token.length < 8 || token.length > 512) {
      throw new Error("Invalid push token");
    }

    const existing = await ctx.db
      .query("pushTokens")
      .withIndex("by_token", (q) => q.eq("token", token))
      .unique();

    if (existing) {
      await ctx.db.patch("pushTokens", existing._id, {
        userId: me._id,
        platform: args.platform,
        updatedAt: Date.now(),
      });
      return existing._id;
    }

    return await ctx.db.insert("pushTokens", {
      userId: me._id,
      token,
      platform: args.platform,
      updatedAt: Date.now(),
    });
  },
});

export const unregisterToken = mutation({
  args: {
    sessionToken: v.string(),
    token: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const existing = await ctx.db
      .query("pushTokens")
      .withIndex("by_token", (q) => q.eq("token", args.token.trim()))
      .unique();
    if (existing && existing.userId === me._id) {
      await ctx.db.delete("pushTokens", existing._id);
    }
    return null;
  },
});

/** Tokens for the current user (settings / debug). */
export const listMine = query({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const rows = await ctx.db
      .query("pushTokens")
      .withIndex("by_user", (q) => q.eq("userId", me._id))
      .take(20);
    return rows.map((r) => ({
      tokenId: r._id,
      platform: r.platform,
      updatedAt: r.updatedAt,
      tokenPreview: `${r.token.slice(0, 8)}…`,
    }));
  },
});

/**
 * Tokens eligible for a conversation notify (respects mute prefs).
 * Used by future FCM/APNs action — returns [] when muted or no tokens.
 */
export const tokensForConversationNotify = internalQuery({
  args: {
    userId: v.id("users"),
    conversationId: v.id("conversations"),
    isMention: v.optional(v.boolean()),
    isEveryone: v.optional(v.boolean()),
  },
  handler: async (ctx, args) => {
    const muted = await isEffectivelyMuted(
      ctx,
      args.userId as Id<"users">,
      args.conversationId as Id<"conversations">,
      {
        isMention: args.isMention === true,
        isEveryone: args.isEveryone === true,
      },
    );
    if (muted) return [];
    const rows = await ctx.db
      .query("pushTokens")
      .withIndex("by_user", (q) => q.eq("userId", args.userId))
      .take(20);
    return rows.map((r) => ({
      token: r.token,
      platform: r.platform,
    }));
  },
});
