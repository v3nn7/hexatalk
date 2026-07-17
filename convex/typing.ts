import { v } from "convex/values";
import { mutation, query } from "./_generated/server";
import { currentUser } from "./session";

const TYPING_TTL_MS = 6000;

export const setTyping = mutation({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
    typing: v.boolean(),
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
      return null;
    }

    const existing = await ctx.db
      .query("typing")
      .withIndex("by_conversation_and_user", (q) =>
        q.eq("conversationId", args.conversationId).eq("userId", me._id),
      )
      .unique();

    if (!args.typing) {
      if (existing) {
        await ctx.db.delete("typing", existing._id);
      }
      return null;
    }

    const expiresAt = Date.now() + TYPING_TTL_MS;
    if (existing) {
      await ctx.db.patch("typing", existing._id, {
        expiresAt,
        displayName: me.displayName,
      });
    } else {
      await ctx.db.insert("typing", {
        conversationId: args.conversationId,
        userId: me._id,
        displayName: me.displayName,
        expiresAt,
      });
    }
    return null;
  },
});

export const whoIsTyping = query({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const now = Date.now();

    const rows = await ctx.db
      .query("typing")
      .withIndex("by_conversation", (q) =>
        q.eq("conversationId", args.conversationId),
      )
      .take(20);

    return rows
      .filter((row) => row.userId !== me._id && row.expiresAt > now)
      .map((row) => row.displayName);
  },
});
