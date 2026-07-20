import { v } from "convex/values";
import { internalMutation, mutation } from "./_generated/server";
import { currentUser } from "./session";

// The desktop client heartbeats on every 5s UI tick; the "online" window
// used by friends/admin queries is 90s. Skipping redundant writes keeps a
// per-user write rate of ~1/30s instead of 1/5s with no visibility change.
const HEARTBEAT_WRITE_INTERVAL_MS = 30_000;

export const heartbeat = mutation({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const now = Date.now();
    const existing = await ctx.db
      .query("presence")
      .withIndex("by_userId", (q) => q.eq("userId", me._id))
      .unique();

    if (existing) {
      if (now - existing.lastSeenAt < HEARTBEAT_WRITE_INTERVAL_MS) {
        return null;
      }
      await ctx.db.patch("presence", existing._id, { lastSeenAt: now });
    } else {
      await ctx.db.insert("presence", { userId: me._id, lastSeenAt: now });
    }
    return null;
  },
});

/**
 * Cron job: drop presence rows whose user no longer exists (orphans left
 * behind by account deletion). lastSeenAt itself is user-visible ("last
 * seen"), so live rows are never pruned by age.
 */
export const cleanupOrphaned = internalMutation({
  args: {},
  handler: async (ctx) => {
    const rows = await ctx.db.query("presence").take(1000);
    let deleted = 0;
    for (const row of rows) {
      const user = await ctx.db.get("users", row.userId);
      if (!user) {
        await ctx.db.delete("presence", row._id);
        deleted += 1;
      }
    }
    return { deleted };
  },
});
