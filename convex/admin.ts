import { v } from "convex/values";
import {
  internalMutation,
  mutation,
  query,
} from "./_generated/server";
import {
  requireAdmin,
  requireStaff,
  platformRank,
  platformRole,
  isPinnedOwner,
  isPinnedOwnerUsername,
} from "./session";

// Same online window the friends/presence queries use, so "online now" in
// the admin dashboard matches what users see elsewhere.
const ONLINE_MS = 90_000;

/**
 * CLI-only batch wipe of Convex file storage blobs left after a table wipe
 * (`npx convex import --replace-all` clears documents but not `_storage`).
 *
 *   npx convex run internal.admin.wipeAllStorage '{"confirm":"WIPE_ALL_STORAGE"}' --prod
 *   # re-run until { deleted: 0, done: true }
 */
export const wipeAllStorage = internalMutation({
  args: { confirm: v.string() },
  handler: async (ctx, args) => {
    if (args.confirm !== "WIPE_ALL_STORAGE") {
      throw new Error('Pass confirm: "WIPE_ALL_STORAGE"');
    }
    const batch = await ctx.db.system.query("_storage").take(100);
    let deleted = 0;
    for (const file of batch) {
      await ctx.storage.delete(file._id);
      deleted += 1;
    }
    return { deleted, done: batch.length < 100 };
  },
});

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
 * - Pinned owner (OWNER_USERNAMES in session.ts) rank is permanent — cannot be changed
 * - Cannot assign "owner" via panel (only pinned list / code)
 * - Granting/revoking "admin" is owner-only; admins manage users/moderators
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

    // Equal rank is off-limits too: admins manage regular users and
    // moderators, never other admins — only the owner outranks an admin.
    if (platformRank(target) >= platformRank(admin)) {
      throw new Error("You can't change someone of equal or higher rank");
    }

    // Granting the admin rank is owner-only (revoking it is covered by
    // the rank check above, since only the owner outranks an admin).
    if (args.role === "admin" && platformRole(admin) !== "owner") {
      throw new Error("Only the instance owner can grant admin");
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

/**
 * Platform-wide counters for the admin dashboard header. On-demand query
 * (called when the Admin tab opens), read-only.
 */
export const adminStats = query({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    await requireStaff(ctx, args.sessionToken);
    const users = await ctx.db.query("users").take(1000);

    const presenceRows = await ctx.db.query("presence").take(2000);
    const now = Date.now();
    const onlineIds = new Set(
      presenceRows
        .filter((p) => now - p.lastSeenAt < ONLINE_MS)
        .map((p) => String(p.userId)),
    );

    let totalUsers = 0;
    let bots = 0;
    let banned = 0;
    let staff = 0;
    let online = 0;
    for (const u of users) {
      if (u.isBot) {
        bots++;
        continue;
      }
      totalUsers++;
      if (u.banned) banned++;
      if (platformRank(u) >= 50) staff++;
      if (onlineIds.has(String(u._id))) online++;
    }

    const servers = await ctx.db.query("servers").take(2000);

    return {
      totalUsers,
      online,
      banned,
      staff,
      bots,
      servers: servers.length,
    };
  },
});

/**
 * Expanded detail for one user, shown in the admin panel's per-user
 * drawer. On-demand query. Staff only.
 */
export const adminUserDetail = query({
  args: { sessionToken: v.string(), userId: v.id("users") },
  handler: async (ctx, args) => {
    await requireStaff(ctx, args.sessionToken);
    const user = await ctx.db.get("users", args.userId);
    if (!user) throw new Error("User not found");

    const presence = await ctx.db
      .query("presence")
      .withIndex("by_userId", (q) => q.eq("userId", args.userId))
      .unique();
    const lastSeenAt = presence?.lastSeenAt ?? 0;
    const online = lastSeenAt > 0 && Date.now() - lastSeenAt < ONLINE_MS;

    const avatarImageUrl = user.avatarStorageId
      ? (await ctx.storage.getUrl(user.avatarStorageId)) ?? ""
      : "";

    const memberships = await ctx.db
      .query("serverMembers")
      .withIndex("by_user", (q) => q.eq("userId", args.userId))
      .take(50);
    const serverNames: string[] = [];
    for (const m of memberships) {
      const server = await ctx.db.get("servers", m.serverId);
      if (server) serverNames.push(server.name);
    }

    const friendsFrom = await ctx.db
      .query("friendRequests")
      .withIndex("by_from_and_status", (q) =>
        q.eq("fromUserId", args.userId).eq("status", "accepted"),
      )
      .take(500);
    const friendsTo = await ctx.db
      .query("friendRequests")
      .withIndex("by_to_and_status", (q) =>
        q.eq("toUserId", args.userId).eq("status", "accepted"),
      )
      .take(500);

    return {
      userId: user._id,
      username: user.username,
      displayName: user.displayName,
      role: platformRole(user),
      banned: user.banned ?? false,
      isBot: user.isBot ?? false,
      bio: user.bio ?? "",
      statusMessage: user.statusMessage ?? "",
      avatarColor: user.avatarColor ?? "",
      avatarImageUrl,
      createdAt: user._creationTime,
      online,
      lastSeenAt,
      serverNames,
      friendCount: friendsFrom.length + friendsTo.length,
    };
  },
});

/**
 * Force-logout: revoke every session for a user without banning them.
 * Staff only, with the same rank guard as ban (can't revoke equal/higher
 * rank or the pinned owner).
 */
export const adminRevokeSessions = mutation({
  args: { sessionToken: v.string(), userId: v.id("users") },
  handler: async (ctx, args) => {
    const staff = await requireStaff(ctx, args.sessionToken);
    if (args.userId === staff._id) {
      throw new Error("Use the normal logout for your own account");
    }
    const target = await ctx.db.get("users", args.userId);
    if (!target) throw new Error("User not found");
    if (isPinnedOwner(target) || isPinnedOwnerUsername(target.username)) {
      throw new Error("The platform owner can't be force-logged-out");
    }
    if (platformRank(target) >= platformRank(staff)) {
      throw new Error("You can't force-logout someone of equal or higher rank");
    }
    const sessions = await ctx.db
      .query("sessions")
      .withIndex("by_userId", (q) => q.eq("userId", args.userId))
      .take(100);
    for (const session of sessions) {
      await ctx.db.delete("sessions", session._id);
    }
    return sessions.length;
  },
});
