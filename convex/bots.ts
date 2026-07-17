import { v } from "convex/values";
import {
  action,
  internalMutation,
  internalQuery,
  mutation,
  query,
} from "./_generated/server";
import { internal } from "./_generated/api";
import { currentUser } from "./session";
import { Id, Doc } from "./_generated/dataModel";
import { hashPassword, randomHex, timingSafeEqual } from "./auth";

const SESSION_TTL_MS = 30 * 24 * 60 * 60 * 1000;
const BOT_TOKEN_PREFIX = "tbot_";

function slugifyBotName(raw: string): string {
  return raw
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9_]+/g, "_")
    .replace(/^_+|_+$/g, "")
    .slice(0, 24);
}

/** Create a bot account. Returns the plaintext token once — store it safely. */
export const create = mutation({
  args: {
    sessionToken: v.string(),
    name: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (me.isBot) {
      throw new Error("Bots cannot create other bots");
    }

    const displayName = args.name.trim().slice(0, 50);
    if (displayName.length < 2) {
      throw new Error("Bot must have a name (min 2 characters)");
    }

    let base = slugifyBotName(displayName);
    if (base.length < 2) base = "bot";
    let username = `bot_${base}`;
    // Ensure unique username.
    for (let i = 0; i < 8; i++) {
      const existing = await ctx.db
        .query("users")
        .withIndex("by_username", (q) => q.eq("username", username))
        .unique();
      if (!existing) break;
      username = `bot_${base}_${randomHex(2)}`;
    }

    const plainToken = BOT_TOKEN_PREFIX + randomHex(24);
    const salt = randomHex(16);
    // Store a SHA-256 of token+salt via same PBKDF2 path as passwords.
    // (hashPassword is async WebCrypto — call through internal? It's exported from auth.)
    // We can't call hashPassword from mutation if it's pure crypto in same module — it works in Convex mutations.
    const passwordHash = await hashPassword(plainToken, salt);

    const botId = await ctx.db.insert("users", {
      username,
      displayName,
      salt,
      passwordHash,
      role: "user",
      isBot: true,
      botOwnerId: me._id,
      avatarColor: "#33FF66",
      storeChatHistory: true,
    });

    return {
      botId,
      username,
      displayName,
      token: plainToken,
    };
  },
});

export const listMine = query({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const bots = await ctx.db
      .query("users")
      .withIndex("by_botOwner", (q) => q.eq("botOwnerId", me._id))
      .take(50);
    return bots
      .filter((b) => b.isBot)
      .map((b) => ({
        botId: b._id,
        username: b.username,
        displayName: b.displayName,
        avatarColor: b.avatarColor ?? "#33FF66",
      }));
  },
});

export const rename = mutation({
  args: {
    sessionToken: v.string(),
    botId: v.id("users"),
    name: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const bot = await ctx.db.get("users", args.botId);
    if (!bot || !bot.isBot || bot.botOwnerId !== me._id) {
      throw new Error("Bot not found");
    }
    const displayName = args.name.trim().slice(0, 50);
    if (displayName.length < 2) {
      throw new Error("Bot must have a name (min 2 characters)");
    }
    await ctx.db.patch("users", bot._id, { displayName });
    return null;
  },
});

export const regenerateToken = mutation({
  args: {
    sessionToken: v.string(),
    botId: v.id("users"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const bot = await ctx.db.get("users", args.botId);
    if (!bot || !bot.isBot || bot.botOwnerId !== me._id) {
      throw new Error("Bot not found");
    }
    const plainToken = BOT_TOKEN_PREFIX + randomHex(24);
    const salt = randomHex(16);
    const passwordHash = await hashPassword(plainToken, salt);
    await ctx.db.patch("users", bot._id, { salt, passwordHash });
    // Kill existing sessions for this bot.
    const sessions = await ctx.db
      .query("sessions")
      .withIndex("by_userId", (q) => q.eq("userId", bot._id))
      .take(100);
    for (const s of sessions) {
      await ctx.db.delete("sessions", s._id);
    }
    return { token: plainToken };
  },
});

export const destroy = mutation({
  args: {
    sessionToken: v.string(),
    botId: v.id("users"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const bot = await ctx.db.get("users", args.botId);
    if (!bot || !bot.isBot || bot.botOwnerId !== me._id) {
      throw new Error("Bot not found");
    }
    // Leave all servers.
    const memberships = await ctx.db
      .query("serverMembers")
      .withIndex("by_user", (q) => q.eq("userId", bot._id))
      .take(200);
    for (const m of memberships) {
      await ctx.db.delete("serverMembers", m._id);
    }
    const convMemberships = await ctx.db
      .query("conversationMembers")
      .withIndex("by_user", (q) => q.eq("userId", bot._id))
      .take(500);
    for (const m of convMemberships) {
      await ctx.db.delete("conversationMembers", m._id);
    }
    const sessions = await ctx.db
      .query("sessions")
      .withIndex("by_userId", (q) => q.eq("userId", bot._id))
      .take(100);
    for (const s of sessions) {
      await ctx.db.delete("sessions", s._id);
    }
    await ctx.db.patch("users", bot._id, {
      banned: true,
      displayName: `[deleted bot]`,
    });
    return null;
  },
});

/** Invite a bot to a server by username (bot_…). Owner/admin only. */
export const inviteToServer = mutation({
  args: {
    sessionToken: v.string(),
    serverId: v.id("servers"),
    botUsername: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const server = await ctx.db.get("servers", args.serverId);
    if (!server) throw new Error("Server not found");
    if (server.ownerId !== me._id && me.role !== "admin") {
      throw new Error("Only the server owner can invite bots");
    }

    const username = args.botUsername.trim().toLowerCase().replace(/^@/, "");
    const bot = await ctx.db
      .query("users")
      .withIndex("by_username", (q) => q.eq("username", username))
      .unique();
    if (!bot || !bot.isBot) {
      throw new Error("Bot not found (use the bot username, e.g. bot_helper)");
    }
    if (!bot.displayName || bot.displayName.trim().length < 2) {
      throw new Error("Bot rejected: missing name");
    }
    if (bot.banned) {
      throw new Error("This bot is disabled");
    }

    const existing = await ctx.db
      .query("serverMembers")
      .withIndex("by_server_and_user", (q) =>
        q.eq("serverId", args.serverId).eq("userId", bot._id),
      )
      .unique();
    if (existing) {
      return bot._id;
    }

    await ctx.db.insert("serverMembers", {
      serverId: args.serverId,
      userId: bot._id,
      joinedAt: Date.now(),
    });

    const channels = await ctx.db
      .query("conversations")
      .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
      .take(200);
    for (const channel of channels) {
      await ctx.db.insert("conversationMembers", {
        conversationId: channel._id,
        userId: bot._id,
      });
    }

    return bot._id;
  },
});

export const kickFromServer = mutation({
  args: {
    sessionToken: v.string(),
    serverId: v.id("servers"),
    botId: v.id("users"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const server = await ctx.db.get("servers", args.serverId);
    if (!server || (server.ownerId !== me._id && me.role !== "admin")) {
      throw new Error("Only the server owner can kick bots");
    }
    const bot = await ctx.db.get("users", args.botId);
    if (!bot || !bot.isBot) throw new Error("Not a bot");

    const membership = await ctx.db
      .query("serverMembers")
      .withIndex("by_server_and_user", (q) =>
        q.eq("serverId", args.serverId).eq("userId", args.botId),
      )
      .unique();
    if (membership) {
      await ctx.db.delete("serverMembers", membership._id);
    }
    const channels = await ctx.db
      .query("conversations")
      .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
      .take(200);
    for (const channel of channels) {
      const row = await ctx.db
        .query("conversationMembers")
        .withIndex("by_conversation_and_user", (q) =>
          q.eq("conversationId", channel._id).eq("userId", args.botId),
        )
        .unique();
      if (row) await ctx.db.delete("conversationMembers", row._id);
    }
    return null;
  },
});

// --- Bot login (no GUI) ---

export const getBotByUsername = internalQuery({
  args: { username: v.string() },
  handler: async (ctx, args) => {
    return await ctx.db
      .query("users")
      .withIndex("by_username", (q) => q.eq("username", args.username))
      .unique();
  },
});

export const createBotSession = internalMutation({
  args: { userId: v.id("users"), token: v.string(), expiresAt: v.number() },
  handler: async (ctx, args) => {
    await ctx.db.insert("sessions", {
      userId: args.userId,
      token: args.token,
      expiresAt: args.expiresAt,
    });
  },
});

export const loginWithUsername = action({
  args: {
    username: v.string(),
    token: v.string(),
  },
  handler: async (ctx, args): Promise<{
    sessionToken: string;
    botId: Id<"users">;
    username: string;
    displayName: string;
  }> => {
    const username = args.username.trim().toLowerCase().replace(/^@/, "");
    const token = args.token.trim();
    if (!token.startsWith(BOT_TOKEN_PREFIX)) {
      throw new Error("Invalid bot token");
    }

    const bot: Doc<"users"> | null = await ctx.runQuery(
      internal.bots.getBotByUsername,
      { username },
    );
    if (!bot || !bot.isBot) {
      throw new Error("Bot not found");
    }
    if (bot.banned) {
      throw new Error("Bot is disabled");
    }
    if (!bot.displayName || bot.displayName.trim().length < 2) {
      throw new Error("Bot rejected: missing name");
    }

    const attemptHash = await hashPassword(token, bot.salt);
    if (!timingSafeEqual(attemptHash, bot.passwordHash)) {
      throw new Error("Invalid bot token");
    }

    const sessionToken = randomHex(32);
    await ctx.runMutation(internal.bots.createBotSession, {
      userId: bot._id,
      token: sessionToken,
      expiresAt: Date.now() + SESSION_TTL_MS,
    });

    return {
      sessionToken,
      botId: bot._id,
      username: bot.username,
      displayName: bot.displayName,
    };
  },
});

/** Convenience: bot sends a message to a channel it is in. */
export const sendMessage = mutation({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
    body: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (!me.isBot) {
      throw new Error("Only bots use this endpoint (humans use messages:send)");
    }
    if (!me.displayName || me.displayName.trim().length < 2) {
      throw new Error("Bot rejected: missing name");
    }
    const body = args.body.trim();
    if (body.length === 0) throw new Error("Empty message");
    if (body.length > 4000) throw new Error("Message too long");

    const membership = await ctx.db
      .query("conversationMembers")
      .withIndex("by_conversation_and_user", (q) =>
        q.eq("conversationId", args.conversationId).eq("userId", me._id),
      )
      .unique();
    if (!membership) {
      throw new Error("Bot is not a member of this channel");
    }

    const conversation = await ctx.db.get("conversations", args.conversationId);
    if (!conversation || conversation.kind !== "channel") {
      throw new Error("Bots can only post in server channels");
    }

    await ctx.db.insert("messages", {
      conversationId: args.conversationId,
      authorId: me._id,
      authorName: me.displayName,
      authorAvatarColor: me.avatarColor,
      body,
    });
    await ctx.db.patch("conversations", args.conversationId, {
      lastMessageAt: Date.now(),
    });
    return null;
  },
});

export const listServerChannels = query({
  args: {
    sessionToken: v.string(),
    serverId: v.id("servers"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (!me.isBot) throw new Error("Bot session required");
    const membership = await ctx.db
      .query("serverMembers")
      .withIndex("by_server_and_user", (q) =>
        q.eq("serverId", args.serverId).eq("userId", me._id),
      )
      .unique();
    if (!membership) throw new Error("Bot is not on this server");

    const channels = await ctx.db
      .query("conversations")
      .withIndex("by_server", (q) => q.eq("serverId", args.serverId))
      .take(200);
    return channels.map((c) => ({
      conversationId: c._id,
      name: c.name ?? "channel",
      channelType: c.channelType ?? "text",
    }));
  },
});

export const listRecentMessages = query({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
    limit: v.optional(v.number()),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (!me.isBot) throw new Error("Bot session required");
    const membership = await ctx.db
      .query("conversationMembers")
      .withIndex("by_conversation_and_user", (q) =>
        q.eq("conversationId", args.conversationId).eq("userId", me._id),
      )
      .unique();
    if (!membership) throw new Error("Bot is not a member of this channel");

    const limit = Math.min(Math.max(args.limit ?? 50, 1), 100);
    const messages = await ctx.db
      .query("messages")
      .withIndex("by_conversation", (q) =>
        q.eq("conversationId", args.conversationId),
      )
      .order("desc")
      .take(limit);

    return messages.map((m) => ({
      id: m._id,
      authorId: m.authorId,
      authorName: m.authorName,
      body: m.deleted ? "" : m.body,
      sentAt: m._creationTime,
      deleted: m.deleted ?? false,
    }));
  },
});

export const me = query({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (!me.isBot) throw new Error("Bot session required");
    return {
      botId: me._id,
      username: me.username,
      displayName: me.displayName,
      isBot: true,
    };
  },
});
