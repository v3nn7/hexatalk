import { v } from "convex/values";
import { mutation, query } from "./_generated/server";
import {
  requireAdmin,
  requireStaff,
  platformRank,
  platformRole,
  isPinnedOwner,
  isPinnedOwnerUsername,
} from "./session";

export const listUsers = query({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    await requireStaff(ctx, args.sessionToken);
    const users = await ctx.db.query("users").take(300);
    return users
      .filter((u) => !u.isBot)
      .map((user) => ({
        userId: user._id,
        username: user.username,
        displayName: user.displayName,
        role: platformRole(user),
        banned: user.banned ?? false,
        roleLocked: isPinnedOwner(user),
      }));
  },
});

/**
 * Set platform role. Only admin/owner can promote/demote.
 * - Cannot demote yourself
 * - Pinned owner (v3nn7) rank is permanent — cannot be changed
 * - Cannot assign "owner" via panel (only pinned list / code)
 * - Moderators cannot set roles at all
 */
export const setRole = mutation({
  args: {
    sessionToken: v.string(),
    userId: v.id("users"),
    role: v.union(
      v.literal("user"),
      v.literal("moderator"),
      v.literal("admin"),
    ),
  },
  handler: async (ctx, args) => {
    const admin = await requireAdmin(ctx, args.sessionToken);
    if (args.userId === admin._id) {
      throw new Error("You can't change your own role");
    }
    const target = await ctx.db.get("users", args.userId);
    if (!target) throw new Error("User not found");
    if (target.isBot) throw new Error("Bots don't have platform roles");

    if (isPinnedOwner(target) || isPinnedOwnerUsername(target.username)) {
      // Ensure DB stays correct even if someone tries to demote.
      if (target.role !== "owner") {
        await ctx.db.patch("users", target._id, { role: "owner" });
      }
      throw new Error("Owner rank is permanent and cannot be removed");
    }

    if (platformRank(target) > platformRank(admin)) {
      throw new Error("You can't change someone of higher rank");
    }

    // Only owner can grant admin (admins can grant mod/user).
    if (args.role === "admin" && admin.role !== "owner" && admin.role !== "admin") {
      throw new Error("Only admins can grant admin");
    }

    await ctx.db.patch("users", args.userId, { role: args.role });
    return null;
  },
});

export const setBanned = mutation({
  args: {
    sessionToken: v.string(),
    userId: v.id("users"),
    banned: v.boolean(),
  },
  handler: async (ctx, args) => {
    const staff = await requireStaff(ctx, args.sessionToken);
    if (args.userId === staff._id) {
      throw new Error("You can't ban yourself");
    }
    const target = await ctx.db.get("users", args.userId);
    if (!target) throw new Error("User not found");

    if (isPinnedOwner(target) || isPinnedOwnerUsername(target.username)) {
      throw new Error("The platform owner cannot be banned");
    }

    // Moderators cannot ban admins or other moderators / owner.
    if (platformRank(target) >= platformRank(staff)) {
      throw new Error("You can't ban someone with equal or higher rank");
    }

    await ctx.db.patch("users", args.userId, { banned: args.banned });
    if (args.banned) {
      const sessions = await ctx.db
        .query("sessions")
        .withIndex("by_userId", (q) => q.eq("userId", args.userId))
        .take(50);
      for (const session of sessions) {
        await ctx.db.delete("sessions", session._id);
      }
    }
    return null;
  },
});
