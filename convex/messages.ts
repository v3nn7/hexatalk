import { v } from "convex/values";
import { mutation, internalMutation, query, MutationCtx, QueryCtx } from "./_generated/server";
import { currentUser, isBlockedEitherWay, platformRank } from "./session";
import { Doc, Id } from "./_generated/dataModel";
import { conversationAllowsStorage } from "./prefs";
import { Perm, channelPermissions, requireChannelAccess, requirePerm } from "./roles";

async function requireMembership(
  ctx: QueryCtx | MutationCtx,
  conversationId: Id<"conversations">,
  userId: Id<"users">,
) {
  const membership = await ctx.db
    .query("conversationMembers")
    .withIndex("by_conversation_and_user", (q) =>
      q.eq("conversationId", conversationId).eq("userId", userId),
    )
    .unique();
  if (!membership) {
    throw new Error("You're not a member of this chat");
  }
  return membership;
}

/** Remove ALL reaction rows of a message (loops past the 200-row page
 * size so a message with more reactions doesn't leak orphan rows). */
async function deleteReactionsForMessage(
  ctx: MutationCtx,
  messageId: Id<"messages">,
) {
  for (;;) {
    const rows = await ctx.db
      .query("reactions")
      .withIndex("by_message", (q) => q.eq("messageId", messageId))
      .take(200);
    if (rows.length === 0) break;
    for (const row of rows) {
      await ctx.db.delete("reactions", row._id);
    }
    if (rows.length < 200) break;
  }
}

/** Remove the edit-history snapshots of a message (purge / clear / wipe). */
async function deleteEditHistoryForMessage(
  ctx: MutationCtx,
  messageId: Id<"messages">,
) {
  const rows = await ctx.db
    .query("messageEditHistory")
    .withIndex("by_message", (q) => q.eq("messageId", messageId))
    .take(50);
  for (const row of rows) {
    await ctx.db.delete("messageEditHistory", row._id);
  }
}

export const REACTION_EMOJIS = ["👍", "❤️", "😂", "😮", "😢", "🔥", "🎉", "👀"];

export const list = query({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
    // Optional "load older" pagination: only messages created BEFORE this
    // ms-epoch timestamp are returned. Omit for the latest page (the Rust
    // client subscribes without it).
    before: v.optional(v.number()),
    // Page size, 1-100, default 100.
    limit: v.optional(v.number()),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await requireChannelAccess(
      ctx,
      args.conversationId,
      me._id,
      Perm.VIEW_CHANNELS,
    );

    const isAdmin = platformRank(me) >= 100;
    const limit = Math.min(Math.max(Math.floor(args.limit ?? 100), 1), 100);
    const messages = await ctx.db
      .query("messages")
      .withIndex("by_conversation", (q) => {
        const base = q.eq("conversationId", args.conversationId);
        return args.before !== undefined
          ? base.lt("_creationTime", args.before)
          : base;
      })
      .order("desc")
      .take(limit);

    // Avatar images are looked up live (not denormalized onto the message)
    // so a profile photo change shows up on the sender's older messages too.
    // Cache per-author within this page since one author usually sent
    // several of the messages being returned.
    const authorMeta = new Map<
      string,
      { url: string; isBot: boolean; plusActive: boolean }
    >();
    async function resolveAuthor(authorId: Id<"users">) {
      const cached = authorMeta.get(authorId);
      if (cached !== undefined) return cached;
      const author = await ctx.db.get("users", authorId);
      const url = author?.avatarStorageId
        ? ((await ctx.storage.getUrl(author.avatarStorageId)) ?? "")
        : "";
      const plusActive =
        !!author &&
        typeof author.plusExpiresAt === "number" &&
        author.plusExpiresAt > Date.now();
      const meta = {
        url,
        isBot: author?.isBot === true,
        plusActive,
      };
      authorMeta.set(authorId, meta);
      return meta;
    }

    return Promise.all(
      messages.reverse().map(async (message) => {
        const deleted = message.deleted ?? false;
        const canSeeReal = isAdmin || message.authorId === me._id || !deleted;

        let attachmentUrl = "";
        if (canSeeReal && message.attachmentStorageId) {
          attachmentUrl =
            (await ctx.storage.getUrl(message.attachmentStorageId)) ?? "";
        }

        let reactions: { emoji: string; count: number; reactedByMe: boolean }[] =
          [];
        if (canSeeReal) {
          const rows = await ctx.db
            .query("reactions")
            .withIndex("by_message", (q) => q.eq("messageId", message._id))
            .take(200);
          const byEmoji = new Map<string, { count: number; reactedByMe: boolean }>();
          for (const row of rows) {
            const entry = byEmoji.get(row.emoji) ?? { count: 0, reactedByMe: false };
            entry.count += 1;
            if (row.userId === me._id) {
              entry.reactedByMe = true;
            }
            byEmoji.set(row.emoji, entry);
          }
          reactions = Array.from(byEmoji.entries()).map(([emoji, entry]) => ({
            emoji,
            ...entry,
          }));
        }

        let replyTo: {
          authorName: string;
          snippet: string;
          encrypted: boolean;
        } | null = null;
        if (canSeeReal && message.replyToMessageId) {
          const target = await ctx.db.get("messages", message.replyToMessageId);
          if (target) {
            const targetDeleted = target.deleted ?? false;
            const targetEncrypted = target.encrypted ?? false;
            // Encrypted bodies are opaque ciphertext blobs -- truncating
            // one server-side would just corrupt it. The full blob goes
            // out instead, and the client decrypts + truncates it.
            const snippet = targetDeleted
              ? "Message deleted"
              : targetEncrypted
                ? target.body
                : target.body.length > 80
                  ? `${target.body.slice(0, 80)}...`
                  : target.body;
            replyTo = {
              authorName: target.authorName,
              snippet,
              encrypted: !targetDeleted && targetEncrypted,
            };
          }
        }

        const author = await resolveAuthor(message.authorId);
        return {
          id: message._id,
          authorId: message.authorId,
          authorName: message.authorName,
          authorAvatarColor: message.authorAvatarColor ?? "",
          authorAvatarImageUrl: author.url,
          authorIsBot: author.isBot,
          authorPlusActive: author.plusActive,
          body: canSeeReal ? message.body : "Message deleted",
          kind: message.kind ?? "text",
          encrypted: canSeeReal && (message.encrypted ?? false),
          attachmentUrl,
          reactions,
          replyTo,
          deleted,
          edited: message.editedAt !== undefined,
          pinned: message.pinned ?? false,
          sentAt: message._creationTime,
        };
      }),
    );
  },
});

export const toggleReaction = mutation({
  args: {
    sessionToken: v.string(),
    messageId: v.id("messages"),
    emoji: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (!REACTION_EMOJIS.includes(args.emoji)) {
      throw new Error("Unsupported reaction");
    }
    const message = await ctx.db.get("messages", args.messageId);
    if (!message) {
      throw new Error("Message not found");
    }
    await requireMembership(ctx, message.conversationId, me._id);

    const existing = await ctx.db
      .query("reactions")
      .withIndex("by_message_and_user_and_emoji", (q) =>
        q
          .eq("messageId", args.messageId)
          .eq("userId", me._id)
          .eq("emoji", args.emoji),
      )
      .unique();
    if (existing) {
      await ctx.db.delete("reactions", existing._id);
    } else {
      await ctx.db.insert("reactions", {
        messageId: args.messageId,
        userId: me._id,
        emoji: args.emoji,
        createdAt: Date.now(),
      });
    }
    return null;
  },
});

const MAX_ATTACHMENT_BYTES = 5 * 1024 * 1024;

// Discord-style cap so a channel can't accumulate unbounded pins.
const MAX_PINS_PER_CONVERSATION = 50;

/**
 * Who may pin/unpin mirrors `remove`: the message author or a platform
 * admin -- plus, for server channels, the server owner (the closest thing
 * a server has to a channel admin in this codebase).
 */
async function canPinMessage(
  ctx: MutationCtx,
  me: Doc<"users">,
  message: Doc<"messages">,
): Promise<boolean> {
  if (message.authorId === me._id || me.role === "admin") {
    return true;
  }
  const conversation = await ctx.db.get("conversations", message.conversationId);
  if (conversation?.kind === "channel" && conversation.serverId) {
    const server = await ctx.db.get("servers", conversation.serverId);
    if (server?.ownerId === me._id) {
      return true;
    }
  }
  return false;
}

export const pinMessage = mutation({
  args: {
    sessionToken: v.string(),
    messageId: v.id("messages"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const message = await ctx.db.get("messages", args.messageId);
    if (!message) {
      throw new Error("Message not found");
    }
    if (message.deleted) {
      throw new Error("You can't pin a deleted message");
    }
    if (message.kind === "call") {
      throw new Error("Call history can't be pinned");
    }
    await requireMembership(ctx, message.conversationId, me._id);
    if (!(await canPinMessage(ctx, me, message))) {
      throw new Error("You don't have permission to pin this message");
    }
    if (message.pinned) {
      return null;
    }

    const existingPins = await ctx.db
      .query("messages")
      .withIndex("by_conversation_and_pinned", (q) =>
        q.eq("conversationId", message.conversationId).eq("pinned", true),
      )
      .take(MAX_PINS_PER_CONVERSATION);
    if (existingPins.length >= MAX_PINS_PER_CONVERSATION) {
      throw new Error(
        `This chat already has ${MAX_PINS_PER_CONVERSATION} pinned messages`,
      );
    }

    await ctx.db.patch("messages", message._id, {
      pinned: true,
      pinnedAt: Date.now(),
      pinnedBy: me._id,
    });
    return null;
  },
});

export const unpinMessage = mutation({
  args: {
    sessionToken: v.string(),
    messageId: v.id("messages"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const message = await ctx.db.get("messages", args.messageId);
    if (!message) {
      throw new Error("Message not found");
    }
    await requireMembership(ctx, message.conversationId, me._id);
    if (!(await canPinMessage(ctx, me, message))) {
      throw new Error("You don't have permission to unpin this message");
    }
    if (!message.pinned) {
      return null;
    }
    await ctx.db.patch("messages", message._id, {
      pinned: undefined,
      pinnedAt: undefined,
      pinnedBy: undefined,
    });
    return null;
  },
});

/**
 * Pinned messages of one conversation for the header panel. Newest pin
 * first. Deleted pins drop out entirely (their bodies are tombstoned in
 * `list` too). Encrypted bodies go out whole -- truncating a ciphertext
 * blob would corrupt it; the client decrypts + truncates (same convention
 * as reply snippets in `list`).
 */
export const listPinned = query({
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

    const messages = await ctx.db
      .query("messages")
      .withIndex("by_conversation_and_pinned", (q) =>
        q.eq("conversationId", args.conversationId).eq("pinned", true),
      )
      .take(MAX_PINS_PER_CONVERSATION);

    return messages
      .filter((m) => !m.deleted && m.kind !== "call")
      .sort((a, b) => (b.pinnedAt ?? 0) - (a.pinnedAt ?? 0))
      .slice(0, MAX_PINS_PER_CONVERSATION)
      .map((m) => {
        const encrypted = m.encrypted ?? false;
        const snippet = encrypted
          ? m.body
          : m.body.length > 80
            ? `${m.body.slice(0, 80)}...`
            : m.body;
        return {
          id: m._id,
          authorId: m.authorId,
          authorName: m.authorName,
          snippet,
          encrypted,
          pinned: true,
          sentAt: m._creationTime,
        };
      });
  },
});

export const generateAttachmentUploadUrl = mutation({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    await currentUser(ctx, args.sessionToken);
    return await ctx.storage.generateUploadUrl();
  },
});

// Encrypted bodies are base64 ratchet blobs (header + GCM ciphertext), which
// run longer than the plaintext they hold -- give them more headroom than
// the plain-text cap used for groups/channels.
const MAX_ENCRYPTED_BODY_LENGTH = 48_000;
const MAX_PLAINTEXT_BODY_LENGTH = 4000;

export const send = mutation({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
    body: v.string(),
    attachmentStorageId: v.optional(v.id("_storage")),
    replyToMessageId: v.optional(v.id("messages")),
    encrypted: v.optional(v.boolean()),
    // Mention metadata, parsed client-side from the PLAINTEXT body (the
    // client owns the plaintext for E2EE chats). Optional: older clients
    // don't send these at all.
    mentionUserIds: v.optional(v.array(v.id("users"))),
    mentionEveryone: v.optional(v.boolean()),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const myMembership = await requireMembership(
      ctx,
      args.conversationId,
      me._id,
    );

    const conversation = await ctx.db.get("conversations", args.conversationId);
    if (!conversation) {
      throw new Error("Conversation not found");
    }

    // Server channels: respect overwrites + announcement staff-only write.
    let channelPerms = 0;
    if (conversation.kind === "channel") {
      channelPerms = await channelPermissions(ctx, args.conversationId, me._id);
      if ((channelPerms & Perm.SEND_MESSAGES) !== Perm.SEND_MESSAGES) {
        throw new Error(
          conversation.isAnnouncement
            ? "Only staff can post in announcements"
            : "You don't have permission to send messages here",
        );
      }
    }

    // DMs: honor blocks either way. Support DMs need no special case here —
    // they ride on the same exemption openSupportDm relies on, since
    // isBlockedEitherWay ignores blocks involving protected staff (platform
    // admins/owners).
    if (conversation.kind === "direct") {
      const members = await ctx.db
        .query("conversationMembers")
        .withIndex("by_conversation", (q) =>
          q.eq("conversationId", args.conversationId),
        )
        .take(10);
      const otherId = members.find((m) => m.userId !== me._id)?.userId;
      if (otherId && (await isBlockedEitherWay(ctx, me._id, otherId))) {
        throw new Error("You can't message this user");
      }
    }

    // DMs: live traffic is peerseal; optional encrypted body is legacy TKR3
    // or plaintext durable history. Groups/channels: encrypted bodies use
    // TGK1 conversation keys (see groupKeys.ts / crypto.rs).
    const kind = conversation.kind;
    if (args.encrypted && kind !== "direct" && kind !== "group" && kind !== "channel") {
      throw new Error("This chat type does not support encrypted messages");
    }

    // Encrypted bodies are opaque ciphertext -- trimming whitespace off an
    // encrypted blob would corrupt it, and an empty check doesn't mean
    // anything for one either (a base64 blob is never actually empty).
    const body = args.encrypted ? args.body : args.body.trim();
    if (!args.encrypted && body.length === 0 && !args.attachmentStorageId) {
      return null;
    }
    if (args.encrypted && body.length === 0) {
      throw new Error("Encrypted message body is required");
    }
    const maxLength = args.encrypted
      ? MAX_ENCRYPTED_BODY_LENGTH
      : MAX_PLAINTEXT_BODY_LENGTH;
    if (body.length > maxLength) {
      throw new Error("Message is too long");
    }

    if (args.attachmentStorageId) {
      const metadata = await ctx.db.system.get(
        "_storage",
        args.attachmentStorageId,
      );
      if (!metadata || metadata.size > MAX_ATTACHMENT_BYTES) {
        await ctx.storage.delete(args.attachmentStorageId);
        throw new Error("Attachment must be smaller than 5MB");
      }
    }

    // Ephemeral: any member opted out of storage (global or per-chat).
    if (!(await conversationAllowsStorage(ctx, args.conversationId))) {
      if (args.attachmentStorageId) {
        try {
          await ctx.storage.delete(args.attachmentStorageId);
        } catch {
          /* ignore */
        }
      }
      return { stored: false };
    }

    let replyToMessageId = args.replyToMessageId;
    if (replyToMessageId) {
      const target = await ctx.db.get("messages", replyToMessageId);
      if (!target || target.conversationId !== args.conversationId) {
        replyToMessageId = undefined;
      }
    }

    // Sanitize mention metadata: only real members of this conversation can
    // be recorded as mentioned (the client computes these, but never trust
    // it blindly), and @everyone only pings in channels/groups -- in 1:1
    // DMs the token is just text. Point-lookup per mentioned id instead of
    // scanning the member list (bounded: max 50 mention ids).
    let mentionUserIds: Id<"users">[] | undefined;
    if (args.mentionUserIds && args.mentionUserIds.length > 0) {
      const candidates = [...new Set(args.mentionUserIds)].slice(0, 50);
      const checks = await Promise.all(
        candidates.map((id) =>
          ctx.db
            .query("conversationMembers")
            .withIndex("by_conversation_and_user", (q) =>
              q.eq("conversationId", args.conversationId).eq("userId", id),
            )
            .unique(),
        ),
      );
      const filtered = candidates.filter((_, i) => checks[i] !== null);
      if (filtered.length > 0) {
        mentionUserIds = filtered;
      }
    }
    // @everyone pings the whole chat, so it takes more than plain
    // membership. roles.ts has no MENTION_EVERYONE bit, so channels fall
    // back to MANAGE_CHANNELS (the server owner passes via ALL_PERMS) and
    // groups allow only their creator.
    let mentionEveryone: true | undefined;
    if (
      args.mentionEveryone === true &&
      (kind === "channel" || kind === "group")
    ) {
      const mayPingEveryone =
        kind === "channel"
          ? (channelPerms & Perm.MANAGE_CHANNELS) === Perm.MANAGE_CHANNELS
          : conversation.createdBy === me._id;
      if (!mayPingEveryone) {
        throw new Error("You don't have permission to mention everyone");
      }
      mentionEveryone = true;
    }

    await ctx.db.insert("messages", {
      conversationId: args.conversationId,
      authorId: me._id,
      authorName: me.displayName,
      authorAvatarColor: me.avatarColor,
      body,
      attachmentStorageId: args.attachmentStorageId,
      replyToMessageId,
      encrypted: args.encrypted ? true : undefined,
      mentionUserIds,
      mentionEveryone,
    });
    const now = Date.now();
    await ctx.db.patch("conversations", args.conversationId, {
      lastMessageAt: now,
    });
    // Discord behavior: your own send is read by definition. Without this
    // the conversation flips to "unread" for the sender (lastMessageAt
    // just moved past their lastReadAt) until the client calls markRead.
    await ctx.db.patch("conversationMembers", myMembership._id, {
      lastReadAt: now,
    });
    return { stored: true };
  },
});

export const edit = mutation({
  args: {
    sessionToken: v.string(),
    messageId: v.id("messages"),
    body: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const message = await ctx.db.get("messages", args.messageId);
    if (!message) {
      throw new Error("Message not found");
    }
    if (message.kind === "call") {
      throw new Error("Call history can't be edited");
    }
    if (message.authorId !== me._id) {
      throw new Error("You can only edit your own messages");
    }
    if (message.deleted) {
      throw new Error("You can't edit a deleted message");
    }
    // Same membership gate as remove: a user who left the conversation can
    // no longer mutate its messages (or probe message ids).
    await requireMembership(ctx, message.conversationId, me._id);

    // Whether this message is encrypted is fixed at send time and never
    // changes; the client is responsible for re-encrypting an edit to a
    // message that was originally encrypted, same as it does when sending.
    const isEncrypted = message.encrypted ?? false;
    const body = isEncrypted ? args.body : args.body.trim();
    if (!isEncrypted && body.length === 0) {
      throw new Error("Message can't be empty");
    }
    const maxLength = isEncrypted
      ? MAX_ENCRYPTED_BODY_LENGTH
      : MAX_PLAINTEXT_BODY_LENGTH;
    if (body.length > maxLength) {
      throw new Error("Message is too long");
    }

    // Snapshot the previous body for the edit-history audit trail BEFORE
    // overwriting it (capped at 10 entries per message).
    const historyRows = await ctx.db
      .query("messageEditHistory")
      .withIndex("by_message", (q) => q.eq("messageId", message._id))
      .take(50);
    if (historyRows.length >= 10) {
      const oldestFirst = [...historyRows].sort(
        (a, b) => a.editedAt - b.editedAt,
      );
      for (const row of oldestFirst.slice(0, historyRows.length - 9)) {
        await ctx.db.delete("messageEditHistory", row._id);
      }
    }
    await ctx.db.insert("messageEditHistory", {
      messageId: message._id,
      editorId: me._id,
      previousBody: message.body,
      editedAt: Date.now(),
    });

    await ctx.db.patch("messages", message._id, {
      body,
      editedAt: Date.now(),
    });
    return null;
  },
});

/**
 * Previous bodies of one message, newest edit first. Only the message
 * author or a platform admin may read the trail (for encrypted messages
 * the snapshots are ciphertext -- the client decrypts them like bodies).
 */
export const listEditHistory = query({
  args: {
    sessionToken: v.string(),
    messageId: v.id("messages"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const message = await ctx.db.get("messages", args.messageId);
    if (!message) {
      throw new Error("Message not found");
    }
    if (message.authorId !== me._id && me.role !== "admin") {
      throw new Error("You can only view the edit history of your own messages");
    }
    const rows = await ctx.db
      .query("messageEditHistory")
      .withIndex("by_message", (q) => q.eq("messageId", args.messageId))
      .take(20);
    return rows
      .sort((a, b) => b.editedAt - a.editedAt)
      .map((row) => ({
        previousBody: row.previousBody,
        editedAt: row.editedAt,
        encrypted: message.encrypted ?? false,
      }));
  },
});

export const remove = mutation({
  args: {
    sessionToken: v.string(),
    messageId: v.id("messages"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const message = await ctx.db.get("messages", args.messageId);
    if (!message) {
      throw new Error("Message not found");
    }
    const isAdmin = platformRank(me) >= 100;
    // Non-admins must actually belong to the conversation to mutate its
    // messages (previously any authenticated user with a message id could
    // tombstone their own old messages after leaving -- and probe ids).
    if (!isAdmin) {
      await requireMembership(ctx, message.conversationId, me._id);
    }
    if (message.kind === "call") {
      if (!isAdmin) {
        throw new Error("Only an admin can remove call history");
      }
    } else if (message.authorId !== me._id && !isAdmin) {
      throw new Error("You don't have permission to delete this message");
    }

    await ctx.db.patch("messages", message._id, {
      deleted: true,
      deletedAt: Date.now(),
      deletedBy: me._id,
    });
    return null;
  },
});

export const purge = mutation({
  args: {
    sessionToken: v.string(),
    messageId: v.id("messages"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (platformRank(me) < 100) {
      throw new Error("Only an admin can permanently delete a message");
    }
    const message = await ctx.db.get("messages", args.messageId);
    if (!message) {
      return null;
    }
    if (message.attachmentStorageId) {
      await ctx.storage.delete(message.attachmentStorageId);
    }
    await deleteReactionsForMessage(ctx, message._id);
    await deleteEditHistoryForMessage(ctx, message._id);
    await ctx.db.delete("messages", message._id);
    return null;
  },
});

/**
 * Hard-delete every message in one conversation (plus reactions / attachment
 * blobs) for all members. Used by the client "Clear chat" action so both
 * sides stop seeing Convex-backed history. Call repeatedly until
 * `done: true` if a channel has a huge backlog.
 */
export const clearConversation = mutation({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await requireMembership(ctx, args.conversationId, me._id);

    // Wiping a whole server channel's history is a destructive manage
    // action — plain members may only clear their own DMs / groups.
    const conversation = await ctx.db.get("conversations", args.conversationId);
    if (conversation?.kind === "channel" && conversation.serverId) {
      await requirePerm(ctx, conversation.serverId, me._id, Perm.MANAGE_CHANNELS);
    }

    const batch = await ctx.db
      .query("messages")
      .withIndex("by_conversation", (q) =>
        q.eq("conversationId", args.conversationId),
      )
      .take(400);

    let purged = 0;
    for (const message of batch) {
      if (message.attachmentStorageId) {
        try {
          await ctx.storage.delete(message.attachmentStorageId);
        } catch {
          // Storage object may already be gone.
        }
      }
      await deleteReactionsForMessage(ctx, message._id);
      await deleteEditHistoryForMessage(ctx, message._id);
      await ctx.db.delete("messages", message._id);
      purged += 1;
    }

    if (batch.length < 400) {
      await ctx.db.patch("conversations", args.conversationId, {
        lastMessageAt: undefined,
      });
    }

    return { purged, done: batch.length < 400 };
  },
});

async function purgeMessageBatch(ctx: MutationCtx) {
  const batch = await ctx.db.query("messages").take(400);
  let purged = 0;
  for (const message of batch) {
    if (message.attachmentStorageId) {
      try {
        await ctx.storage.delete(message.attachmentStorageId);
      } catch {
        // Storage object may already be gone; still drop the message row.
      }
    }
    await deleteReactionsForMessage(ctx, message._id);
    await deleteEditHistoryForMessage(ctx, message._id);
    await ctx.db.delete("messages", message._id);
    purged += 1;
  }

  // Clear last-message timestamps so sidebars don't show stale previews.
  if (purged > 0) {
    const conversations = await ctx.db.query("conversations").take(200);
    for (const conversation of conversations) {
      if (conversation.lastMessageAt !== undefined) {
        await ctx.db.patch("conversations", conversation._id, {
          lastMessageAt: undefined,
        });
      }
    }
  }

  return { purged, done: batch.length < 400 };
}

/**
 * One-shot wipe of every message (and its reactions / attachment blobs) so
 * a crypto protocol upgrade can start from an empty history. Admin-only.
 * Processes up to 400 messages per call -- invoke repeatedly until
 * `done: true` if the deployment has a large backlog.
 */
export const purgeAllHistory = mutation({
  args: {
    sessionToken: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (platformRank(me) < 100) {
      throw new Error("Only an admin can wipe chat history");
    }
    return await purgeMessageBatch(ctx);
  },
});

/**
 * CLI-only wipe for the E2EE protocol upgrade. Marked `internalMutation` so
 * it is NOT reachable from the public client API (a public mutation with only
 * a literal confirm string would let anyone wipe all history). Invoke it from
 * the CLI as an internal function:
 *
 *   npx convex run internal:messages:purgeAllHistoryConfirm '{"confirm":"WIPE_ALL_MESSAGES"}'
 */
export const purgeAllHistoryConfirm = internalMutation({
  args: {
    confirm: v.literal("WIPE_ALL_MESSAGES"),
  },
  handler: async (ctx) => {
    return await purgeMessageBatch(ctx);
  },
});

/**
 * Lightweight message search across conversations the user belongs to.
 * Scans recent plaintext (skips encrypted bodies). Best-effort, not a full index.
 */
export const search = query({
  args: {
    sessionToken: v.string(),
    query: v.string(),
    conversationId: v.optional(v.id("conversations")),
    limit: v.optional(v.number()),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const q = args.query.trim().toLowerCase();
    if (q.length < 2) return [];
    const limit = Math.min(Math.max(args.limit ?? 30, 1), 50);

    type Hit = {
      messageId: Id<"messages">;
      conversationId: Id<"conversations">;
      authorName: string;
      body: string;
      sentAt: number;
      conversationName: string;
    };
    const hits: Hit[] = [];

    const scanConversation = async (conversationId: Id<"conversations">) => {
      if (hits.length >= limit) return;
      const membership = await ctx.db
        .query("conversationMembers")
        .withIndex("by_conversation_and_user", (q2) =>
          q2.eq("conversationId", conversationId).eq("userId", me._id),
        )
        .unique();
      if (!membership) return;
      const conversation = await ctx.db.get("conversations", conversationId);
      if (!conversation) return;
      if (conversation.kind === "channel") {
        const perms = await channelPermissions(ctx, conversationId, me._id);
        if ((perms & Perm.VIEW_CHANNELS) !== Perm.VIEW_CHANNELS) return;
      }
      const conversationName =
        conversation.name ??
        (conversation.kind === "direct" ? "Direct" : "Chat");

      const recent = await ctx.db
        .query("messages")
        .withIndex("by_conversation", (q2) =>
          q2.eq("conversationId", conversationId),
        )
        .order("desc")
        .take(200);
      for (const m of recent) {
        if (hits.length >= limit) break;
        if (m.deleted || m.encrypted || m.kind === "call") continue;
        if (!m.body.toLowerCase().includes(q)) continue;
        hits.push({
          messageId: m._id,
          conversationId,
          authorName: m.authorName,
          body: m.body.slice(0, 200),
          sentAt: m._creationTime,
          conversationName,
        });
      }
    };

    if (args.conversationId) {
      await scanConversation(args.conversationId);
    } else {
      const memberships = await ctx.db
        .query("conversationMembers")
        .withIndex("by_user", (q2) => q2.eq("userId", me._id))
        .take(80);
      for (const m of memberships) {
        if (hits.length >= limit) break;
        await scanConversation(m.conversationId);
      }
    }

    return hits.sort((a, b) => b.sentAt - a.sentAt).slice(0, limit);
  },
});
