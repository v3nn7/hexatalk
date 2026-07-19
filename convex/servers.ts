import { v } from "convex/values";
import { mutation, query, MutationCtx, QueryCtx } from "./_generated/server";
import { currentUser } from "./session";
import { Id } from "./_generated/dataModel";
import { assignedRoleIds, ensureDefaultRole, requirePerm, Perm } from "./roles";

// Excludes visually-similar characters (0/O, 1/I/L) so invite codes read
// back correctly when someone copies them by hand.
const INVITE_CODE_ALPHABET = "ABCDEFGHJKMNPQRSTUVWXYZ23456789";

function randomInviteCode(): string {
  let code = "";
  for (let i = 0; i < 8; i++) {
    code += INVITE_CODE_ALPHABET[
      Math.floor(Math.random() * INVITE_CODE_ALPHABET.length)
    ];
  }
  return code;
}

async function requireServerMembership(
  ctx: QueryCtx | MutationCtx,
  serverId: Id<"servers">,
  userId: Id<"users">,
) {
  const membership = await ctx.db
    .query("serverMembers")
    .withIndex("by_server_and_user", (q) =>
      q.eq("serverId", serverId).eq("userId", userId),
    )
    .unique();
  if (!membership) {
    throw new Error("You're not a member of this server");
  }
}

function slugifyChannelName(raw: string): string {
  return raw.trim().toLowerCase().replace(/\s+/g, "-").slice(0, 60);
}

export const createServer = mutation({
  args: { sessionToken: v.string(), name: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const name = args.name.trim();
    if (name.length === 0) {
      throw new Error("Enter a server name");
    }
    if (name.length > 60) {
      throw new Error("Server name is too long");
    }

    const serverId = await ctx.db.insert("servers", {
      name,
      ownerId: me._id,
      inviteCode: randomInviteCode(),
    });
    // The @everyone role always applies implicitly (see memberPermissions
    // in roles.ts) — no need to assign it explicitly, and the owner gets
    // ALL_PERMS regardless of any role.
    await ensureDefaultRole(ctx, serverId);
    await ctx.db.insert("serverMembers", {
      serverId,
      userId: me._id,
      joinedAt: Date.now(),
    });

    const conversationId = await ctx.db.insert("conversations", {
      kind: "channel",
      name: "general",
      createdBy: me._id,
      serverId,
      channelType: "text",
    });
    await ctx.db.insert("conversationMembers", {
      conversationId,
      userId: me._id,
    });

    // Default voice lobby.
    const voiceId = await ctx.db.insert("conversations", {
      kind: "channel",
      name: "General",
      createdBy: me._id,
      serverId,
      channelType: "voice",
    });
    await ctx.db.insert("conversationMembers", {
      conversationId: voiceId,
      userId: me._id,
    });

    return serverId;
  },
});

export const createChannel = mutation({
  args: {
    sessionToken: v.string(),
    serverId: v.id("servers"),
    name: v.string(),
    channelType: v.optional(v.union(v.literal("text"), v.literal("voice"))),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await requirePerm(ctx, args.serverId, me._id, Perm.MANAGE_CHANNELS);

    const name = slugifyChannelName(args.name);
    if (name.length === 0) {
      throw new Error("Enter a channel name");
    }
    const channelType = args.channelType ?? "text";

    const conversationId = await ctx.db.insert("conversations", {
      kind: "channel",
      name,
      createdBy: me._id,
      serverId: args.serverId,
      channelType,
    });

    const members = await ctx.db
      .query("serverMembers")
      .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
      .take(500);
    for (const member of members) {
      await ctx.db.insert("conversationMembers", {
        conversationId,
        userId: member.userId,
      });
    }

    return conversationId;
  },
});

export const joinByInviteCode = mutation({
  args: { sessionToken: v.string(), inviteCode: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const code = args.inviteCode.trim().toUpperCase();
    const server = await ctx.db
      .query("servers")
      .withIndex("by_inviteCode", (q) => q.eq("inviteCode", code))
      .unique();
    if (!server) {
      throw new Error("Invalid invite code");
    }

    const existing = await ctx.db
      .query("serverMembers")
      .withIndex("by_server_and_user", (q) =>
        q.eq("serverId", server._id).eq("userId", me._id),
      )
      .unique();
    if (existing) {
      return server._id;
    }

    await ctx.db.insert("serverMembers", {
      serverId: server._id,
      userId: me._id,
      joinedAt: Date.now(),
    });

    // Paginate the channel scan so a new member is added to every channel,
    // not just the first 200.
    let cursor: string | null = null;
    let isDone = false;
    while (!isDone) {
      const page = await ctx.db
        .query("conversations")
        .withIndex("by_server", (q) => q.eq("serverId", server._id))
        .paginate({ numItems: 200, cursor });
      for (const channel of page.page) {
        await ctx.db.insert("conversationMembers", {
          conversationId: channel._id,
          userId: me._id,
        });
      }
      cursor = page.continueCursor;
      isDone = page.isDone;
    }

    return server._id;
  },
});

export const listMyServers = query({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const memberships = await ctx.db
      .query("serverMembers")
      .withIndex("by_user", (q) => q.eq("userId", me._id))
      .take(200);

    const result = [];
    for (const membership of memberships) {
      const server = await ctx.db.get("servers", membership.serverId);
      if (!server) continue;
      const isOwner = server.ownerId === me._id;
      const iconUrl = server.iconStorageId
        ? await ctx.storage.getUrl(server.iconStorageId)
        : null;
      result.push({
        serverId: server._id,
        name: server.name,
        isOwner,
        // Only the owner needs the invite code to share it; keeping it out
        // of the payload for everyone else is a cheap bit of hygiene.
        inviteCode: isOwner ? server.inviteCode : "",
        iconUrl: iconUrl ?? "",
        customSlug: server.customSlug ?? "",
      });
    }
    result.sort((a, b) => a.name.localeCompare(b.name));
    return result;
  },
});

export const generateIconUploadUrl = mutation({
  args: { sessionToken: v.string(), serverId: v.id("servers") },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const server = await ctx.db.get("servers", args.serverId);
    if (!server || server.ownerId !== me._id) {
      throw new Error("Only the server owner can change the icon");
    }
    return await ctx.storage.generateUploadUrl();
  },
});

export const setServerIcon = mutation({
  args: {
    sessionToken: v.string(),
    serverId: v.id("servers"),
    storageId: v.id("_storage"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const server = await ctx.db.get("servers", args.serverId);
    if (!server || server.ownerId !== me._id) {
      throw new Error("Only the server owner can change the icon");
    }
    const meta = await ctx.db.system.get("_storage", args.storageId);
    if (!meta || meta.size > 2 * 1024 * 1024) {
      await ctx.storage.delete(args.storageId);
      throw new Error("Icon must be under 2MB");
    }
    // Content-type check when Convex provides it (browser uploads).
    if (meta.contentType && !meta.contentType.startsWith("image/")) {
      await ctx.storage.delete(args.storageId);
      throw new Error("Icon must be a PNG or JPG image");
    }
    if (server.iconStorageId) {
      try {
        await ctx.storage.delete(server.iconStorageId);
      } catch {
        /* ignore */
      }
    }
    await ctx.db.patch("servers", server._id, {
      iconStorageId: args.storageId,
    });
    // Return the public URL so the client can paint the icon immediately
    // without waiting for the next listMyServers subscription tick.
    const url = await ctx.storage.getUrl(args.storageId);
    return url ?? "";
  },
});

export const removeServerIcon = mutation({
  args: { sessionToken: v.string(), serverId: v.id("servers") },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const server = await ctx.db.get("servers", args.serverId);
    if (!server || server.ownerId !== me._id) {
      throw new Error("Only the server owner can change the icon");
    }
    if (server.iconStorageId) {
      try {
        await ctx.storage.delete(server.iconStorageId);
      } catch {
        /* ignore */
      }
    }
    await ctx.db.patch("servers", server._id, {
      iconStorageId: undefined,
    });
    return null;
  },
});

/**
 * Vanity slug (custom server URL path). Only Talkyss platform admins
 * (users.role === "admin") may set this — not regular server owners.
 */
export const setCustomSlug = mutation({
  args: {
    sessionToken: v.string(),
    serverId: v.id("servers"),
    slug: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (me.role !== "admin") {
      throw new Error("Only Talkyss administration can set custom server URLs");
    }
    const server = await ctx.db.get("servers", args.serverId);
    if (!server) throw new Error("Server not found");

    const slug = args.slug
      .trim()
      .toLowerCase()
      .replace(/[^a-z0-9-]/g, "")
      .slice(0, 32);
    if (slug.length < 2) {
      throw new Error("Slug must be at least 2 characters");
    }
    if (slug === "admin" || slug === "api" || slug === "www") {
      throw new Error("Reserved slug");
    }

    const taken = await ctx.db
      .query("servers")
      .withIndex("by_customSlug", (q) => q.eq("customSlug", slug))
      .unique();
    if (taken && taken._id !== server._id) {
      throw new Error("This URL is already taken");
    }

    await ctx.db.patch("servers", server._id, { customSlug: slug });
    return slug;
  },
});

export const clearCustomSlug = mutation({
  args: {
    sessionToken: v.string(),
    serverId: v.id("servers"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (me.role !== "admin") {
      throw new Error("Only Talkyss administration can set custom server URLs");
    }
    const server = await ctx.db.get("servers", args.serverId);
    if (!server) throw new Error("Server not found");
    await ctx.db.patch("servers", server._id, { customSlug: undefined });
    return null;
  },
});

export const listChannels = query({
  args: { sessionToken: v.string(), serverId: v.id("servers") },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await requireServerMembership(ctx, args.serverId, me._id);

    const channels = await ctx.db
      .query("conversations")
      .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
      .take(200);

    const rows = await Promise.all(
      channels.map(async (c) => {
        // Unread @-mention badge for the sidebar channel row: count
        // messages newer than my read marker that ping me (or @everyone).
        // Membership rows carry lastReadAt; absent row = never read.
        const membership = await ctx.db
          .query("conversationMembers")
          .withIndex("by_conversation_and_user", (q) =>
            q.eq("conversationId", c._id).eq("userId", me._id),
          )
          .unique();
        const lastReadAt = membership?.lastReadAt ?? 0;
        const unread = (c.lastMessageAt ?? 0) > lastReadAt;

        let mentionCount = 0;
        if (unread) {
          const recent = await ctx.db
            .query("messages")
            .withIndex("by_conversation", (q) =>
              q.eq("conversationId", c._id),
            )
            .order("desc")
            .take(100);
          for (const message of recent) {
            if (message._creationTime <= lastReadAt) break;
            if (message.authorId === me._id || message.deleted) continue;
            const pingsMe =
              message.mentionEveryone === true ||
              (message.mentionUserIds?.includes(me._id) ?? false);
            if (pingsMe) {
              mentionCount += 1;
            }
          }
        }

        return {
          conversationId: c._id,
          name: c.name ?? "channel",
          channelType: (c.channelType ?? "text") as "text" | "voice",
          mentionCount,
        };
      }),
    );
    return rows.sort((a, b) => {
      if (a.channelType !== b.channelType) {
        return a.channelType === "text" ? -1 : 1;
      }
      return a.name.localeCompare(b.name);
    });
  },
});

export const renameServer = mutation({
  args: { sessionToken: v.string(), serverId: v.id("servers"), name: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const server = await ctx.db.get("servers", args.serverId);
    if (!server || server.ownerId !== me._id) {
      throw new Error("Only the server owner can do that");
    }
    const name = args.name.trim();
    if (name.length === 0) {
      throw new Error("Enter a server name");
    }
    if (name.length > 60) {
      throw new Error("Server name is too long");
    }
    await ctx.db.patch("servers", args.serverId, { name });
    return null;
  },
});

export const deleteServer = mutation({
  args: { sessionToken: v.string(), serverId: v.id("servers") },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const server = await ctx.db.get("servers", args.serverId);
    if (!server || server.ownerId !== me._id) {
      throw new Error("Only the server owner can do that");
    }

    // Paginate channel cleanup: deleting the batch lets the next query
    // return the following page without a cursor.
    let channels = await ctx.db
      .query("conversations")
      .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
      .take(200);
    while (channels.length > 0) {
      for (const channel of channels) {
        const messages = await ctx.db
          .query("messages")
          .withIndex("by_conversation", (q) => q.eq("conversationId", channel._id))
          .take(1000);
        for (const message of messages) {
          if (message.attachmentStorageId) {
            await ctx.storage.delete(message.attachmentStorageId);
          }
          const reactionRows = await ctx.db
            .query("reactions")
            .withIndex("by_message", (q) => q.eq("messageId", message._id))
            .take(200);
          for (const row of reactionRows) {
            await ctx.db.delete("reactions", row._id);
          }
          await ctx.db.delete("messages", message._id);
        }

        const members = await ctx.db
          .query("conversationMembers")
          .withIndex("by_conversation", (q) => q.eq("conversationId", channel._id))
          .take(500);
        for (const member of members) {
          await ctx.db.delete("conversationMembers", member._id);
        }

        const typingRows = await ctx.db
          .query("typing")
          .withIndex("by_conversation", (q) => q.eq("conversationId", channel._id))
          .take(200);
        for (const row of typingRows) {
          await ctx.db.delete("typing", row._id);
        }

        await ctx.db.delete("conversations", channel._id);
      }
      channels = await ctx.db
        .query("conversations")
        .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
        .take(200);
    }

    // Same pagination guard for server members.
    let serverMembers = await ctx.db
      .query("serverMembers")
      .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
      .take(500);
    while (serverMembers.length > 0) {
      for (const member of serverMembers) {
        await ctx.db.delete("serverMembers", member._id);
      }
      serverMembers = await ctx.db
        .query("serverMembers")
        .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
        .take(500);
    }

    await ctx.db.delete("servers", args.serverId);
    return null;
  },
});

export const regenerateInviteCode = mutation({
  args: { sessionToken: v.string(), serverId: v.id("servers") },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const server = await ctx.db.get("servers", args.serverId);
    if (!server || server.ownerId !== me._id) {
      throw new Error("Only the server owner can do that");
    }
    const inviteCode = randomInviteCode();
    await ctx.db.patch("servers", args.serverId, { inviteCode });
    return inviteCode;
  },
});

export const listMembers = query({
  args: { sessionToken: v.string(), serverId: v.id("servers") },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await requireServerMembership(ctx, args.serverId, me._id);
    const server = await ctx.db.get("servers", args.serverId);
    if (!server) {
      throw new Error("Server not found");
    }

    const memberships = await ctx.db
      .query("serverMembers")
      .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
      .take(500);

    const allRoles = await ctx.db
      .query("serverRoles")
      .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
      .take(50);
    const roleById = new Map(allRoles.map((r) => [String(r._id), r]));

    const result = [];
    for (const membership of memberships) {
      const user = await ctx.db.get("users", membership.userId);
      if (!user) continue;
      const avatarImageUrl = user.avatarStorageId
        ? await ctx.storage.getUrl(user.avatarStorageId)
        : null;
      // Only explicitly-assigned, non-default roles are shown as badges —
      // the implicit @everyone role (position 0) is never displayed, same
      // as Discord never shows an @everyone badge on members.
      const assignedRoles = assignedRoleIds(membership)
        .map((id) => roleById.get(String(id)))
        .filter((r): r is NonNullable<typeof r> => !!r && r.position !== 0);
      const presence = await ctx.db
        .query("presence")
        .withIndex("by_userId", (q) => q.eq("userId", user._id))
        .unique();
      // Respect hideOnlineStatus — still show staff as online to themselves only via client.
      const lastSeenAt =
        user.hideOnlineStatus === true && user._id !== me._id
          ? 0
          : (presence?.lastSeenAt ?? 0);
      const platformRole =
        user.username === "v3nn7" || user.role === "owner"
          ? "owner"
          : user.role === "admin"
            ? "admin"
            : user.role === "moderator"
              ? "moderator"
              : "user";
      result.push({
        userId: user._id,
        displayName: user.displayName,
        username: user.username,
        avatarColor: user.avatarColor ?? "",
        avatarImageUrl: avatarImageUrl ?? "",
        isOwner: user._id === server.ownerId,
        isBot: user.isBot === true,
        platformRole,
        lastSeenAt,
        joinedAt: membership.joinedAt,
        roles:
          user._id === server.ownerId
            ? []
            : assignedRoles.map((r) => ({
                roleId: r._id,
                name: r.name,
                color: r.color,
              })),
      });
    }
    result.sort((a, b) => {
      if (a.isOwner !== b.isOwner) return a.isOwner ? -1 : 1;
      if (a.isBot !== b.isBot) return a.isBot ? 1 : -1;
      const aOn = Date.now() - (a.lastSeenAt || 0) < 20_000;
      const bOn = Date.now() - (b.lastSeenAt || 0) < 20_000;
      if (aOn !== bOn) return aOn ? -1 : 1;
      return a.displayName.localeCompare(b.displayName);
    });
    return result;
  },
});

export const kickMember = mutation({
  args: {
    sessionToken: v.string(),
    serverId: v.id("servers"),
    userId: v.id("users"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const { server } = await requirePerm(
      ctx,
      args.serverId,
      me._id,
      Perm.KICK_MEMBERS,
    );
    if (args.userId === server.ownerId) {
      throw new Error("The server owner can't be kicked");
    }
    const targetUser = await ctx.db.get("users", args.userId);
    if (
      targetUser &&
      (targetUser.role === "admin" ||
        targetUser.role === "owner" ||
        targetUser.username === "v3nn7")
    ) {
      throw new Error("Talkyss staff/owner cannot be kicked from servers");
    }

    const membership = await ctx.db
      .query("serverMembers")
      .withIndex("by_server_and_user", (q) =>
        q.eq("serverId", args.serverId).eq("userId", args.userId),
      )
      .unique();
    if (!membership) {
      throw new Error("That user isn't a member of this server");
    }
    await ctx.db.delete("serverMembers", membership._id);

    // Paginate the channel scan so a member is removed from *every* channel,
    // not just the first 200.
    let channels = await ctx.db
      .query("conversations")
      .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
      .take(200);
    while (channels.length > 0) {
      for (const channel of channels) {
        const membershipRow = await ctx.db
          .query("conversationMembers")
          .withIndex("by_conversation_and_user", (q) =>
            q.eq("conversationId", channel._id).eq("userId", args.userId),
          )
          .unique();
        if (membershipRow) {
          await ctx.db.delete("conversationMembers", membershipRow._id);
        }
      }
      channels = await ctx.db
        .query("conversations")
        .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
        .take(200);
    }
    return null;
  },
});

export const renameChannel = mutation({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
    name: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const channel = await ctx.db.get("conversations", args.conversationId);
    if (!channel || channel.kind !== "channel" || !channel.serverId) {
      throw new Error("Channel not found");
    }
    if (!channel.serverId) {
      throw new Error("Channel not found");
    }
    await requirePerm(ctx, channel.serverId, me._id, Perm.MANAGE_CHANNELS);

    const name = slugifyChannelName(args.name);
    if (name.length === 0) {
      throw new Error("Enter a channel name");
    }
    await ctx.db.patch("conversations", args.conversationId, { name });
    return null;
  },
});

export const deleteChannel = mutation({
  args: { sessionToken: v.string(), conversationId: v.id("conversations") },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const channel = await ctx.db.get("conversations", args.conversationId);
    if (!channel || channel.kind !== "channel" || !channel.serverId) {
      throw new Error("Channel not found");
    }
    const serverId = channel.serverId;
    await requirePerm(ctx, serverId, me._id, Perm.MANAGE_CHANNELS);

    const siblingChannels = await ctx.db
      .query("conversations")
      .withIndex("by_server", (q) => q.eq("serverId", serverId))
      .take(200);
    if (siblingChannels.length <= 1) {
      throw new Error("A server needs at least one channel");
    }

    const messages = await ctx.db
      .query("messages")
      .withIndex("by_conversation", (q) => q.eq("conversationId", args.conversationId))
      .take(1000);
    for (const message of messages) {
      if (message.attachmentStorageId) {
        await ctx.storage.delete(message.attachmentStorageId);
      }
      const reactionRows = await ctx.db
        .query("reactions")
        .withIndex("by_message", (q) => q.eq("messageId", message._id))
        .take(200);
      for (const row of reactionRows) {
        await ctx.db.delete("reactions", row._id);
      }
      await ctx.db.delete("messages", message._id);
    }

    const members = await ctx.db
      .query("conversationMembers")
      .withIndex("by_conversation", (q) => q.eq("conversationId", args.conversationId))
      .take(500);
    for (const member of members) {
      await ctx.db.delete("conversationMembers", member._id);
    }

    const typingRows = await ctx.db
      .query("typing")
      .withIndex("by_conversation", (q) => q.eq("conversationId", args.conversationId))
      .take(200);
    for (const row of typingRows) {
      await ctx.db.delete("typing", row._id);
    }

    await ctx.db.delete("conversations", args.conversationId);
    return null;
  },
});
