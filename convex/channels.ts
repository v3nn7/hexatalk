import { v } from "convex/values";
import { mutation, query, MutationCtx, QueryCtx } from "./_generated/server";
import { currentUser } from "./session";
import { Id } from "./_generated/dataModel";
import {
  ALL_PERMS,
  DEFAULT_EVERYONE_PERMS,
  Perm,
  channelPermissions,
  highestRolePosition,
  requirePerm,
  requireServerMember,
} from "./roles";

function slugify(raw: string): string {
  return raw.trim().toLowerCase().replace(/\s+/g, "-").slice(0, 60);
}

/** Permissions an @everyone overwrite may tune (member-facing defaults). */
const EVERYONE_OVERWRITE_PERMS =
  Perm.VIEW_CHANNELS | Perm.SEND_MESSAGES | Perm.CONNECT_VOICE | Perm.SPEAK;

/** Add every server member to a conversation, paginating the member scan
 * so large servers are fully covered (not just the first 500). */
async function addAllServerMembersToConversation(
  ctx: MutationCtx,
  serverId: Id<"servers">,
  conversationId: Id<"conversations">,
) {
  let cursor: string | null = null;
  let isDone = false;
  while (!isDone) {
    const page = await ctx.db
      .query("serverMembers")
      .withIndex("by_server", (q) => q.eq("serverId", serverId))
      .paginate({ numItems: 500, cursor });
    for (const member of page.page) {
      await ctx.db.insert("conversationMembers", {
        conversationId,
        userId: member.userId,
      });
    }
    cursor = page.continueCursor;
    isDone = page.isDone;
  }
}

/** Id of the server's position-0 (@everyone) role, creating it when an old
 * server predates default roles — mirrors ensureDefaultRole in roles.ts. */
async function everyoneRoleId(
  ctx: MutationCtx,
  serverId: Id<"servers">,
): Promise<Id<"serverRoles">> {
  const roles = await ctx.db
    .query("serverRoles")
    .withIndex("by_server", (q) => q.eq("serverId", serverId))
    .take(50);
  const everyone = roles.find((r) => r.position === 0);
  if (everyone) return everyone._id;
  return await ctx.db.insert("serverRoles", {
    serverId,
    name: "everyone",
    color: "#33FF66",
    position: 0,
    permissions: DEFAULT_EVERYONE_PERMS,
  });
}

export const listCategories = query({
  args: { sessionToken: v.string(), serverId: v.id("servers") },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await requireServerMember(ctx, args.serverId, me._id);
    const cats = await ctx.db
      .query("channelCategories")
      .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
      .take(50);
    return cats
      .map((c) => ({
        categoryId: c._id,
        name: c.name,
        position: c.position,
      }))
      .sort((a, b) => a.position - b.position || a.name.localeCompare(b.name));
  },
});

export const createCategory = mutation({
  args: {
    sessionToken: v.string(),
    serverId: v.id("servers"),
    name: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await requirePerm(ctx, args.serverId, me._id, Perm.MANAGE_CHANNELS);
    const name = args.name.trim().slice(0, 40);
    if (!name) throw new Error("Enter a category name");
    const existing = await ctx.db
      .query("channelCategories")
      .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
      .take(50);
    const position =
      existing.reduce((max, c) => Math.max(max, c.position), 0) + 1;
    return await ctx.db.insert("channelCategories", {
      serverId: args.serverId,
      name,
      position,
    });
  },
});

export const renameCategory = mutation({
  args: {
    sessionToken: v.string(),
    categoryId: v.id("channelCategories"),
    name: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const cat = await ctx.db.get("channelCategories", args.categoryId);
    if (!cat) throw new Error("Category not found");
    await requirePerm(ctx, cat.serverId, me._id, Perm.MANAGE_CHANNELS);
    const name = args.name.trim().slice(0, 40);
    if (!name) throw new Error("Enter a category name");
    await ctx.db.patch("channelCategories", args.categoryId, { name });
    return null;
  },
});

export const deleteCategory = mutation({
  args: {
    sessionToken: v.string(),
    categoryId: v.id("channelCategories"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const cat = await ctx.db.get("channelCategories", args.categoryId);
    if (!cat) throw new Error("Category not found");
    await requirePerm(ctx, cat.serverId, me._id, Perm.MANAGE_CHANNELS);
    // Paginate so channels beyond the first 200 are also unassigned.
    let cursor: string | null = null;
    let isDone = false;
    while (!isDone) {
      const page = await ctx.db
        .query("conversations")
        .withIndex("by_server", (q) => q.eq("serverId", cat.serverId))
        .paginate({ numItems: 200, cursor });
      for (const ch of page.page) {
        if (ch.categoryId === args.categoryId) {
          await ctx.db.patch("conversations", ch._id, {
            categoryId: undefined,
          });
        }
      }
      cursor = page.continueCursor;
      isDone = page.isDone;
    }
    await ctx.db.delete("channelCategories", args.categoryId);
    return null;
  },
});

export const setChannelCategory = mutation({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
    categoryId: v.optional(v.id("channelCategories")),
    position: v.optional(v.number()),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const ch = await ctx.db.get("conversations", args.conversationId);
    if (!ch || ch.kind !== "channel" || !ch.serverId) {
      throw new Error("Not a server channel");
    }
    await requirePerm(ctx, ch.serverId, me._id, Perm.MANAGE_CHANNELS);
    if (args.categoryId) {
      const cat = await ctx.db.get("channelCategories", args.categoryId);
      if (!cat || cat.serverId !== ch.serverId) {
        throw new Error("Category not on this server");
      }
    }
    await ctx.db.patch("conversations", args.conversationId, {
      categoryId: args.categoryId,
      ...(args.position !== undefined ? { position: args.position } : {}),
    });
    return null;
  },
});

/**
 * Move a channel up/down within the same channel type (text/voice).
 * Announcement/system channels stay pinned at the top of the text list and
 * cannot be reordered relative to regular channels.
 */
export const moveChannel = mutation({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
    direction: v.union(v.literal("up"), v.literal("down")),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const ch = await ctx.db.get("conversations", args.conversationId);
    if (!ch || ch.kind !== "channel" || !ch.serverId) {
      throw new Error("Not a server channel");
    }
    if (ch.isSystem || ch.isAnnouncement) {
      throw new Error("System / announcements channels stay at the top");
    }
    await requirePerm(ctx, ch.serverId, me._id, Perm.MANAGE_CHANNELS);

    const channelType = ch.channelType ?? "text";
    const all = await ctx.db
      .query("conversations")
      .withIndex("by_server", (q) => q.eq("serverId", ch.serverId!))
      .take(200);

    // Normalize missing positions so swaps work on older servers.
    const peers = all
      .filter(
        (c) =>
          (c.channelType ?? "text") === channelType &&
          !c.isAnnouncement &&
          !c.isSystem,
      )
      .sort((a, b) => {
        const pa = a.position ?? 0;
        const pb = b.position ?? 0;
        if (pa !== pb) return pa - pb;
        return (a.name ?? "").localeCompare(b.name ?? "");
      });

    // Assign dense positions if any missing/duplicate.
    for (let i = 0; i < peers.length; i++) {
      if (peers[i].position !== i) {
        await ctx.db.patch("conversations", peers[i]._id, { position: i });
        peers[i] = { ...peers[i], position: i };
      }
    }

    const idx = peers.findIndex((c) => c._id === args.conversationId);
    if (idx < 0) throw new Error("Channel not found in list");
    const swapWith = args.direction === "up" ? idx - 1 : idx + 1;
    if (swapWith < 0 || swapWith >= peers.length) {
      return null; // already at edge
    }

    const a = peers[idx];
    const b = peers[swapWith];
    const posA = a.position ?? idx;
    const posB = b.position ?? swapWith;
    await ctx.db.patch("conversations", a._id, { position: posB });
    await ctx.db.patch("conversations", b._id, { position: posA });
    return null;
  },
});

export const listOverwrites = query({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const ch = await ctx.db.get("conversations", args.conversationId);
    if (!ch || !ch.serverId) throw new Error("Not a server channel");
    await requireServerMember(ctx, ch.serverId, me._id);
    const rows = await ctx.db
      .query("channelOverwrites")
      .withIndex("by_conversation", (q) =>
        q.eq("conversationId", args.conversationId),
      )
      .take(100);
    return rows.map((r) => ({
      overwriteId: r._id,
      targetType: r.targetType,
      targetId: r.targetId,
      allow: r.allow,
      deny: r.deny,
    }));
  },
});

export const setOverwrite = mutation({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
    targetType: v.union(v.literal("role"), v.literal("member")),
    targetId: v.string(),
    allow: v.number(),
    deny: v.number(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const ch = await ctx.db.get("conversations", args.conversationId);
    if (!ch || ch.kind !== "channel" || !ch.serverId) {
      throw new Error("Not a server channel");
    }
    const { server, membership, perms: myPerms } = await requirePerm(
      ctx,
      ch.serverId,
      me._id,
      Perm.MANAGE_ROLES,
    );

    let allow = args.allow & ALL_PERMS;
    let deny = args.deny & ALL_PERMS;
    if (allow & deny) {
      throw new Error("A permission cannot be both allowed and denied");
    }

    // The overwrite target must belong to this server — otherwise junk
    // overwrites for arbitrary role/user ids could be planted.
    if (args.targetType === "role") {
      const role = await ctx.db.get(
        "serverRoles",
        args.targetId as Id<"serverRoles">,
      );
      if (!role || role.serverId !== ch.serverId) {
        throw new Error("Role not found on this server");
      }
      if (role.position === 0) {
        // The @everyone overwrite may only tune member-facing defaults —
        // it can never grant/revoke management bits server-wide.
        allow &= EVERYONE_OVERWRITE_PERMS;
        deny &= EVERYONE_OVERWRITE_PERMS;
      } else if (
        server.ownerId !== me._id &&
        role.position >= (await highestRolePosition(ctx, membership))
      ) {
        // Discord-style hierarchy, mirrors updateRole in roles.ts.
        throw new Error(
          "You can't set overwrites for a role at or above your highest role",
        );
      }
    } else {
      const target = await ctx.db.get("users", args.targetId as Id<"users">);
      if (!target) {
        throw new Error("User not found");
      }
      await requireServerMember(ctx, ch.serverId, target._id);
    }

    // Never allow permission bits you don't hold yourself (owner bypasses),
    // same rule as updateRole in roles.ts.
    if (server.ownerId !== me._id && (allow & ~myPerms) !== 0) {
      throw new Error("You can't allow permissions you don't have");
    }

    const existing = await ctx.db
      .query("channelOverwrites")
      .withIndex("by_conversation_and_target", (q) =>
        q
          .eq("conversationId", args.conversationId)
          .eq("targetType", args.targetType)
          .eq("targetId", args.targetId),
      )
      .unique();

    if (allow === 0 && deny === 0) {
      if (existing) await ctx.db.delete("channelOverwrites", existing._id);
      return null;
    }

    if (existing) {
      await ctx.db.patch("channelOverwrites", existing._id, { allow, deny });
      return existing._id;
    }
    return await ctx.db.insert("channelOverwrites", {
      conversationId: args.conversationId,
      serverId: ch.serverId,
      targetType: args.targetType,
      targetId: args.targetId,
      allow,
      deny,
    });
  },
});

export const deleteOverwrite = mutation({
  args: {
    sessionToken: v.string(),
    overwriteId: v.id("channelOverwrites"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const row = await ctx.db.get("channelOverwrites", args.overwriteId);
    if (!row) return null;
    await requirePerm(ctx, row.serverId, me._id, Perm.MANAGE_ROLES);
    await ctx.db.delete("channelOverwrites", args.overwriteId);
    return null;
  },
});

export const myChannelPermissions = query({
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
      return { permissions: 0, canSend: false, canView: false };
    }
    const permissions = await channelPermissions(
      ctx,
      args.conversationId,
      me._id,
    );
    return {
      permissions,
      canSend: (permissions & Perm.SEND_MESSAGES) === Perm.SEND_MESSAGES,
      canView: (permissions & Perm.VIEW_CHANNELS) === Perm.VIEW_CHANNELS,
    };
  },
});

/** Mute a conversation or whole server (desktop + push suppress). */
export const setMute = mutation({
  args: {
    sessionToken: v.string(),
    scope: v.union(v.literal("conversation"), v.literal("server")),
    targetId: v.string(),
    muted: v.boolean(),
    mutedUntil: v.optional(v.number()),
    suppressMentions: v.optional(v.boolean()),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (args.scope === "conversation") {
      const membership = await ctx.db
        .query("conversationMembers")
        .withIndex("by_conversation_and_user", (q) =>
          q
            .eq("conversationId", args.targetId as Id<"conversations">)
            .eq("userId", me._id),
        )
        .unique();
      if (!membership) throw new Error("Not a member of this chat");
    } else {
      await requireServerMember(
        ctx,
        args.targetId as Id<"servers">,
        me._id,
      );
    }

    const existing = await ctx.db
      .query("notificationPrefs")
      .withIndex("by_user_and_scope_and_target", (q) =>
        q
          .eq("userId", me._id)
          .eq("scope", args.scope)
          .eq("targetId", args.targetId),
      )
      .unique();

    if (!args.muted) {
      if (existing) await ctx.db.delete("notificationPrefs", existing._id);
      return null;
    }

    // A mute expiry in the past is meaningless — treat it as "no expiry".
    const mutedUntil =
      args.mutedUntil !== undefined && args.mutedUntil > Date.now()
        ? args.mutedUntil
        : undefined;

    const doc = {
      userId: me._id,
      scope: args.scope,
      targetId: args.targetId,
      muted: true,
      mutedUntil,
      suppressMentions: args.suppressMentions ?? false,
      updatedAt: Date.now(),
    };
    if (existing) {
      await ctx.db.patch("notificationPrefs", existing._id, doc);
      return existing._id;
    }
    return await ctx.db.insert("notificationPrefs", doc);
  },
});

export const listMutes = query({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const rows = await ctx.db
      .query("notificationPrefs")
      .withIndex("by_user", (q) => q.eq("userId", me._id))
      .take(200);
    const now = Date.now();
    return rows
      .filter((r) => r.muted && (!r.mutedUntil || r.mutedUntil > now))
      .map((r) => ({
        scope: r.scope,
        targetId: r.targetId,
        mutedUntil: r.mutedUntil ?? 0,
        suppressMentions: r.suppressMentions === true,
      }));
  },
});

export async function isEffectivelyMuted(
  ctx: QueryCtx | MutationCtx,
  userId: Id<"users">,
  conversationId: Id<"conversations">,
  opts?: { isMention?: boolean; isEveryone?: boolean },
): Promise<boolean> {
  const now = Date.now();
  const convPref = await ctx.db
    .query("notificationPrefs")
    .withIndex("by_user_and_scope_and_target", (q) =>
      q
        .eq("userId", userId)
        .eq("scope", "conversation")
        .eq("targetId", String(conversationId)),
    )
    .unique();
  if (convPref?.muted && (!convPref.mutedUntil || convPref.mutedUntil > now)) {
    if (
      (opts?.isMention || opts?.isEveryone) &&
      convPref.suppressMentions !== true
    ) {
      return false;
    }
    return true;
  }

  const conversation = await ctx.db.get("conversations", conversationId);
  if (conversation?.serverId) {
    const serverPref = await ctx.db
      .query("notificationPrefs")
      .withIndex("by_user_and_scope_and_target", (q) =>
        q
          .eq("userId", userId)
          .eq("scope", "server")
          .eq("targetId", String(conversation.serverId)),
      )
      .unique();
    if (
      serverPref?.muted &&
      (!serverPref.mutedUntil || serverPref.mutedUntil > now)
    ) {
      if (
        (opts?.isMention || opts?.isEveryone) &&
        serverPref.suppressMentions !== true
      ) {
        return false;
      }
      return true;
    }
  }
  return false;
}

/** Ensure announcement channel exists (migration helper for old servers). */
export const ensureAnnouncementChannel = mutation({
  args: { sessionToken: v.string(), serverId: v.id("servers") },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await requirePerm(ctx, args.serverId, me._id, Perm.MANAGE_SERVER);

    const channels = await ctx.db
      .query("conversations")
      .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
      .take(200);
    const existing = channels.find((c) => c.isAnnouncement === true);
    if (existing) return existing._id;

    const conversationId = await ctx.db.insert("conversations", {
      kind: "channel",
      name: "announcements",
      createdBy: me._id,
      serverId: args.serverId,
      channelType: "text",
      isAnnouncement: true,
      isSystem: true,
      position: -1000,
    });
    await addAllServerMembersToConversation(
      ctx,
      args.serverId,
      conversationId,
    );
    return conversationId;
  },
});

export const createTextChannel = mutation({
  args: {
    sessionToken: v.string(),
    serverId: v.id("servers"),
    name: v.string(),
    categoryId: v.optional(v.id("channelCategories")),
    channelType: v.optional(v.union(v.literal("text"), v.literal("voice"))),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await requirePerm(ctx, args.serverId, me._id, Perm.MANAGE_CHANNELS);
    const name = slugify(args.name);
    if (!name) throw new Error("Enter a channel name");
    if (args.categoryId) {
      const cat = await ctx.db.get("channelCategories", args.categoryId);
      if (!cat || cat.serverId !== args.serverId) {
        throw new Error("Invalid category");
      }
    }
    const siblings = await ctx.db
      .query("conversations")
      .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
      .take(200);
    const position =
      siblings.reduce((max, c) => Math.max(max, c.position ?? 0), 0) + 1;

    const conversationId = await ctx.db.insert("conversations", {
      kind: "channel",
      name,
      createdBy: me._id,
      serverId: args.serverId,
      channelType: args.channelType ?? "text",
      categoryId: args.categoryId,
      position,
    });
    await addAllServerMembersToConversation(
      ctx,
      args.serverId,
      conversationId,
    );
    return conversationId;
  },
});
