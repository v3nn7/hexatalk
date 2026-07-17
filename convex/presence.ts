import { v } from "convex/values";
import { mutation } from "./_generated/server";
import { currentUser } from "./session";

export const heartbeat = mutation({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const existing = await ctx.db
      .query("presence")
      .withIndex("by_userId", (q) => q.eq("userId", me._id))
      .unique();

    if (existing) {
      await ctx.db.patch("presence", existing._id, { lastSeenAt: Date.now() });
    } else {
      await ctx.db.insert("presence", { userId: me._id, lastSeenAt: Date.now() });
    }
    return null;
  },
});
