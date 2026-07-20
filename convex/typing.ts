import { v } from "convex/values";
import {
  internalMutation,
  mutation,
  query,
} from "./_generated/server";
import { currentUser } from "./session";
import { Perm, requireChannelAccess } from "./roles";

const TYPING_TTL_MS = 6000;
// setTyping(true) re-fires every couple of seconds while the user types;
// rewriting the row every time just churns watchers. Only extend the row
// when less than this much of its TTL remains.
const TYPING_REFRESH_THRESHOLD_MS = 2000;

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
      // Skip the write when the row still has plenty of TTL left (the
      // client re-sends typing every few seconds; without this each ping
      // re-fires every whoIsTyping subscriber with no visible change).
      if (existing.expiresAt - Date.now() > TYPING_REFRESH_THRESHOLD_MS) {
        return null;
      }
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

/**
 * Cron job: delete expired typing rows so the table doesn't accumulate
 * tombstones from clients that went offline without sending typing=false.
 * whoIsTyping already filters them out, this is pure housekeeping.
 */
export const cleanupExpired = internalMutation({
  args: {},
  handler: async (ctx) => {
    const stale = await ctx.db
      .query("typing")
      .withIndex("by_expiresAt", (q) => q.lt("expiresAt", Date.now()))
      .take(500);
    for (const row of stale) {
      await ctx.db.delete("typing", row._id);
    }
    return { deleted: stale.length };
  },
});

export const whoIsTyping = query({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await requireChannelAccess(
      ctx,
      args.conversationId,
      me._id,
      Perm.VIEW_CHANNELS,
    );
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
