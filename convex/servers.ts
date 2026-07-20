import { v } from "convex/values";
import { mutation, query, MutationCtx, QueryCtx } from "./_generated/server";
import { currentUser, platformRank, platformRole } from "./session";
import { Id } from "./_generated/dataModel";
import {
  assignedRoleIds,
  ensureDefaultRole,
  requirePerm,
  Perm,
  DEFAULT_STAFF_PERMS,
  channelPermissions,
  highestRolePosition,
} from "./roles";

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
    // Staff role: can manage channels + post in #announcements.
    await ctx.db.insert("serverRoles", {
      serverId,
      name: "Staff",
      color: "#FAA61A",
      position: 1,
      permissions: DEFAULT_STAFF_PERMS,
    });
    await ctx.db.insert("serverMembers", {
      serverId,
      userId: me._id,
      joinedAt: Date.now(),
    });

    // Always-visible announcements: everyone reads, only staff/owner write.
    const announcementsId = await ctx.db.insert("conversations", {
      kind: "channel",
      name: "announcements",
      createdBy: me._id,
      serverId,
      channelType: "text",
      isAnnouncement: true,
      isSystem: true,
      position: -1000,
    });
    await ctx.db.insert("conversationMembers", {
      conversationId: announcementsId,
      userId: me._id,
    });

    const conversationId = await ctx.db.insert("conversations", {
      kind: "channel",
      name: "general",
      createdBy: me._id,
      serverId,
      channelType: "text",
      position: 0,
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
      position: 1,
    });
    await ctx.db.insert("conversationMembers", {
      conversationId: voiceId,
      userId: me._id,
    });

    // New members land in general (not announcements).
    await ctx.db.patch("servers", serverId, {
      welcomeChannelId: conversationId,
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

    // Paginate the member fan-out so a channel on a large server includes
    // every member, not just the first 500.
    let cursor: string | null = null;
    let isDone = false;
    while (!isDone) {
      const page = await ctx.db
        .query("serverMembers")
        .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
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

    return conversationId;
  },
});

export const joinByInviteCode = mutation({
  args: { sessionToken: v.string(), inviteCode: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    // Bots don't use invite codes — the owner adds them via
    // bots.inviteToServer. Joining by code would let a leaked bot token
    // plant the bot on arbitrary servers.
    if (me.isBot) {
      throw new Error(
        "Bots join servers through their owner's bot invite, not invite codes",
      );
    }
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

    // Owner has paused invites: the code is intentionally dead for new
    // members (checked after the existing-member early-return so people
    // already in the server aren't affected).
    if (server.invitesPaused) {
      throw new Error("This server isn't accepting new members right now");
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
        description: server.description ?? "",
        createdAt: server._creationTime,
        welcomeChannelId: server.welcomeChannelId ?? "",
        invitesPaused: server.invitesPaused ?? false,
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
 * Vanity slug (custom server URL path). Only HexaTalk platform admins
 * (platformRank >= 100, i.e. admins and owners) may set this — not regular
 * server owners.
 */
export const setCustomSlug = mutation({
  args: {
    sessionToken: v.string(),
    serverId: v.id("servers"),
    slug: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (platformRank(me) < 100) {
      throw new Error("Only HexaTalk administration can set custom server URLs");
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
    if (platformRank(me) < 100) {
      throw new Error("Only HexaTalk administration can set custom server URLs");
    }
    const server = await ctx.db.get("servers", args.serverId);
    if (!server) throw new Error("Server not found");
    await ctx.db.patch("servers", server._id, { customSlug: undefined });
    return null;
  },
});

/**
 * Resolves a vanity slug (from a `vyrapp://join/<slug>` deep link) to the
 * info needed for a join-confirmation prompt. Public: called before the
 * client has any server membership, sometimes before login even finishes.
 */
export const resolveCustomSlug = query({
  args: { slug: v.string() },
  handler: async (ctx, args) => {
    const slug = args.slug.trim().toLowerCase();
    const server = await ctx.db
      .query("servers")
      .withIndex("by_customSlug", (q) => q.eq("customSlug", slug))
      .unique();
    if (!server) return null;
    const iconUrl = server.iconStorageId
      ? await ctx.storage.getUrl(server.iconStorageId)
      : null;
    return {
      serverId: server._id,
      name: server.name,
      iconUrl: iconUrl ?? "",
      invitesPaused: server.invitesPaused ?? false,
      inviteCode: server.invitesPaused ? "" : server.inviteCode,
    };
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
        // Permission gate first: channels the member can't see are dropped
        // before the (potentially expensive) unread/mention scan below.
        const perms = await channelPermissions(ctx, c._id, me._id);
        if ((perms & Perm.VIEW_CHANNELS) !== Perm.VIEW_CHANNELS) {
          return null;
        }

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

        const muteRow = await ctx.db
          .query("notificationPrefs")
          .withIndex("by_user_and_scope_and_target", (q) =>
            q
              .eq("userId", me._id)
              .eq("scope", "conversation")
              .eq("targetId", String(c._id)),
          )
          .unique();
        const now = Date.now();
        const muted =
          !!muteRow?.muted &&
          (!muteRow.mutedUntil || muteRow.mutedUntil > now);

        return {
          conversationId: c._id,
          name: c.name ?? "channel",
          channelType: (c.channelType ?? "text") as "text" | "voice",
          mentionCount,
          categoryId: c.categoryId ?? null,
          position: c.position ?? 0,
          isAnnouncement: c.isAnnouncement === true,
          isSystem: c.isSystem === true,
          muted,
          canSend: (perms & Perm.SEND_MESSAGES) === Perm.SEND_MESSAGES,
          permissions: perms,
        };
      }),
    );
    return rows
      .filter((r): r is NonNullable<typeof r> => r !== null)
      .sort((a, b) => {
        // Announcements always pin to the top of text channels.
        if (a.isAnnouncement !== b.isAnnouncement) {
          return a.isAnnouncement ? -1 : 1;
        }
        if (a.channelType !== b.channelType) {
          return a.channelType === "text" ? -1 : 1;
        }
        if (a.position !== b.position) return a.position - b.position;
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

        const voiceRows = await ctx.db
          .query("voiceStates")
          .withIndex("by_conversation", (q) =>
            q.eq("conversationId", channel._id),
          )
          .take(200);
        for (const row of voiceRows) {
          await ctx.db.delete("voiceStates", row._id);
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

    // Server-scoped leftovers: roles, categories, channel overwrites.
    // (Server-scoped notificationPrefs have no by-target index, so they
    // can't be cleaned up efficiently here — orphaned but harmless.)
    const roles = await ctx.db
      .query("serverRoles")
      .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
      .take(100);
    for (const role of roles) {
      await ctx.db.delete("serverRoles", role._id);
    }

    const categories = await ctx.db
      .query("channelCategories")
      .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
      .take(100);
    for (const category of categories) {
      await ctx.db.delete("channelCategories", category._id);
    }

    let overwrites = await ctx.db
      .query("channelOverwrites")
      .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
      .take(500);
    while (overwrites.length > 0) {
      for (const ow of overwrites) {
        await ctx.db.delete("channelOverwrites", ow._id);
      }
      overwrites = await ctx.db
        .query("channelOverwrites")
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

/** Owner-editable "about" blurb. Empty string clears it. */
export const setServerDescription = mutation({
  args: {
    sessionToken: v.string(),
    serverId: v.id("servers"),
    description: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const server = await ctx.db.get("servers", args.serverId);
    if (!server || server.ownerId !== me._id) {
      throw new Error("Only the server owner can do that");
    }
    const description = args.description.trim().slice(0, 300);
    await ctx.db.patch("servers", args.serverId, {
      description: description.length > 0 ? description : undefined,
    });
    return null;
  },
});

/**
 * Hand the server to another member. Irreversible from the old owner's
 * side (the new owner would have to hand it back). The new owner must be a
 * non-bot member of this server.
 */
export const transferOwnership = mutation({
  args: {
    sessionToken: v.string(),
    serverId: v.id("servers"),
    newOwnerId: v.id("users"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const server = await ctx.db.get("servers", args.serverId);
    if (!server || server.ownerId !== me._id) {
      throw new Error("Only the server owner can do that");
    }
    if (args.newOwnerId === me._id) {
      throw new Error("You already own this server");
    }
    const target = await ctx.db.get("users", args.newOwnerId);
    if (!target) throw new Error("User not found");
    if (target.isBot) throw new Error("Bots can't own servers");
    const membership = await ctx.db
      .query("serverMembers")
      .withIndex("by_server_and_user", (q) =>
        q.eq("serverId", args.serverId).eq("userId", args.newOwnerId),
      )
      .unique();
    if (!membership) {
      throw new Error("That user isn't a member of this server");
    }
    await ctx.db.patch("servers", args.serverId, { ownerId: args.newOwnerId });
    return null;
  },
});

/**
 * Channel a new member lands in first. Empty string clears it (falls back
 * to the first text channel). The channel must belong to this server.
 */
export const setWelcomeChannel = mutation({
  args: {
    sessionToken: v.string(),
    serverId: v.id("servers"),
    channelId: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const server = await ctx.db.get("servers", args.serverId);
    if (!server || server.ownerId !== me._id) {
      throw new Error("Only the server owner can do that");
    }
    if (args.channelId.trim() === "") {
      await ctx.db.patch("servers", args.serverId, {
        welcomeChannelId: undefined,
      });
      return null;
    }
    const channel = await ctx.db.get(
      "conversations",
      args.channelId as Id<"conversations">,
    );
    if (!channel || channel.serverId !== args.serverId) {
      throw new Error("That channel isn't part of this server");
    }
    await ctx.db.patch("servers", args.serverId, {
      welcomeChannelId: channel._id,
    });
    return null;
  },
});

/** Toggle whether the public invite code accepts new members. */
export const setInvitesPaused = mutation({
  args: {
    sessionToken: v.string(),
    serverId: v.id("servers"),
    paused: v.boolean(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const server = await ctx.db.get("servers", args.serverId);
    if (!server || server.ownerId !== me._id) {
      throw new Error("Only the server owner can do that");
    }
    await ctx.db.patch("servers", args.serverId, {
      invitesPaused: args.paused,
    });
    return null;
  },
});

/**
 * Read-only server stats for the Overview card. On-demand query (the
 * client calls it once when opening settings, not as a live subscription)
 * so the bounded message scan below never re-runs on every new message.
 */
export const serverStats = query({
  args: { sessionToken: v.string(), serverId: v.id("servers") },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await requireServerMembership(ctx, args.serverId, me._id);

    const channels = await ctx.db
      .query("conversations")
      .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
      .take(200);
    let textChannels = 0;
    let voiceChannels = 0;
    for (const c of channels) {
      if (c.channelType === "voice") voiceChannels++;
      else textChannels++;
    }

    const members = await ctx.db
      .query("serverMembers")
      .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
      .take(1000);
    // Oldest membership = the server's longest-standing member. Find the
    // minimum joinedAt first, then resolve that single user — avoids a
    // users-table read per member.
    let oldestJoinedAt = 0;
    let oldestUserId: Id<"users"> | null = null;
    for (const m of members) {
      if (oldestJoinedAt === 0 || m.joinedAt < oldestJoinedAt) {
        oldestJoinedAt = m.joinedAt;
        oldestUserId = m.userId;
      }
    }
    let oldestName = "";
    if (oldestUserId) {
      const oldestUser = await ctx.db.get("users", oldestUserId);
      if (oldestUser) {
        oldestName = oldestUser.displayName || oldestUser.username;
      }
    }

    const roles = await ctx.db
      .query("serverRoles")
      .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
      .take(100);

    // Bounded message count across channels: cap total scanned so a busy
    // server can't turn this into an unbounded read. "capped" tells the UI
    // to render "5000+".
    const MESSAGE_CAP = 5000;
    let messageCount = 0;
    let capped = false;
    for (const c of channels) {
      if (messageCount >= MESSAGE_CAP) {
        capped = true;
        break;
      }
      const remaining = MESSAGE_CAP - messageCount + 1;
      const msgs = await ctx.db
        .query("messages")
        .withIndex("by_conversation", (q) => q.eq("conversationId", c._id))
        .take(remaining);
      messageCount += msgs.length;
    }
    if (messageCount > MESSAGE_CAP) {
      messageCount = MESSAGE_CAP;
      capped = true;
    }

    return {
      memberCount: members.length,
      textChannels,
      voiceChannels,
      roleCount: roles.length,
      messageCount,
      messagesCapped: capped,
      createdAt: (await ctx.db.get("servers", args.serverId))?._creationTime ?? 0,
      oldestMemberName: oldestName,
      oldestMemberJoinedAt: oldestJoinedAt,
    };
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
      // Centralized in session.ts (pinned owner list lives there).
      const userPlatformRole = platformRole(user);
      const plusActive =
        typeof user.plusExpiresAt === "number" &&
        user.plusExpiresAt > Date.now();
      result.push({
        userId: user._id,
        displayName: user.displayName,
        username: user.username,
        avatarColor: user.avatarColor ?? "",
        avatarImageUrl: avatarImageUrl ?? "",
        isOwner: user._id === server.ownerId,
        isBot: user.isBot === true,
        platformRole: userPlatformRole,
        plusActive,
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
    const { server, membership: myMembership } = await requirePerm(
      ctx,
      args.serverId,
      me._id,
      Perm.KICK_MEMBERS,
    );
    if (args.userId === server.ownerId) {
      throw new Error("The server owner can't be kicked");
    }
    const targetUser = await ctx.db.get("users", args.userId);
    // Platform admins/owner (rank >= 100, including the pinned owner) are
    // unkickable — mirrors isProtectedTarget in session.ts.
    if (targetUser && platformRank(targetUser) >= 100) {
      throw new Error("HexaTalk staff/owner cannot be kicked from servers");
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
    // Discord-style hierarchy: non-owners can only kick members below
    // their own highest role (mirrors toggleRole in roles.ts).
    if (
      server.ownerId !== me._id &&
      (await highestRolePosition(ctx, membership)) >=
        (await highestRolePosition(ctx, myMembership))
    ) {
      throw new Error("You can't kick someone with an equal or higher role");
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
    await requirePerm(ctx, channel.serverId, me._id, Perm.MANAGE_CHANNELS);
    if (channel.isSystem) {
      throw new Error("System channels can't be renamed");
    }

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
    if (channel.isSystem || channel.isAnnouncement) {
      throw new Error("System / announcements channels can't be deleted");
    }

    const siblingChannels = await ctx.db
      .query("conversations")
      .withIndex("by_server", (q) => q.eq("serverId", serverId))
      .take(200);
    const isTextChannel = (channel.channelType ?? "text") === "text";
    if (isTextChannel) {
      // Edge case: a voice-only server is useless, so the last *text*
      // channel is protected — not just the last channel of any type.
      const remainingText = siblingChannels.filter(
        (c) => c._id !== channel._id && (c.channelType ?? "text") === "text",
      );
      if (remainingText.length === 0) {
        throw new Error("A server needs at least one text channel");
      }
    } else if (siblingChannels.length <= 1) {
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

    // Don't leave a dangling welcomeChannelId pointing at the deleted
    // channel — fall back to the first text channel instead.
    const serverDoc = await ctx.db.get("servers", serverId);
    if (serverDoc?.welcomeChannelId === args.conversationId) {
      await ctx.db.patch("servers", serverId, { welcomeChannelId: undefined });
    }

    await ctx.db.delete("conversations", args.conversationId);
    return null;
  },
});
